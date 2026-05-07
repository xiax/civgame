use ahash::AHashMap;
use bevy::ecs::entity::Entities;
use bevy::prelude::*;

use crate::simulation::construction::{Blueprint, BlueprintMap};
use crate::simulation::faction::{FactionData, FactionMember, FactionRegistry, SOLO};
use crate::simulation::goals::{AgentGoal, Personality};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{PersonAI, Profession};
use crate::simulation::projects::{compute_priority, ProjectPhase, Projects, PRIORITY_PLAYER};
use crate::simulation::schedule::SimClock;
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::technology::CROP_CULTIVATION;

pub type JobId = u32;
pub type RecipeId = u8;

/// Faction-directed job categories. The workforce budget enforces per-kind caps
/// so the chief can balance how many workers each role consumes.
///
/// The construction pipeline splits into three independent stages:
/// - `Stockpile` — bring a Good into faction storage (anticipatory + reactive).
/// - `Haul` — withdraw a specific Good from storage and deliver it into a
///   specific blueprint's deposit slot.
/// - `Build` — perform labor ticks at a blueprint whose deposits are filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// Gather a Good in the world and deposit it to faction storage. Covers
    /// food (`JobProgress::Calories`) and construction materials
    /// (`JobProgress::Stockpile`).
    Stockpile,
    /// Withdraw a Good from faction storage and deliver it into a specific
    /// blueprint's deposit slot. Pure transport — does not gather.
    Haul,
    Farm,
    Craft,
    Build,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Stockpile => "Stockpile",
            JobKind::Haul => "Haul",
            JobKind::Farm => "Farm",
            JobKind::Craft => "Craft",
            JobKind::Build => "Build",
        }
    }

    pub fn to_goal(self) -> AgentGoal {
        match self {
            JobKind::Stockpile => AgentGoal::GatherFood,
            JobKind::Haul => AgentGoal::Haul,
            JobKind::Farm => AgentGoal::Farm,
            JobKind::Craft => AgentGoal::Craft,
            JobKind::Build => AgentGoal::Build,
        }
    }
}

/// Capability gate: does this faction meet the prerequisites to perform `kind`
/// at all? Independent of "is there work to do right now" — that remains a
/// per-posting check (e.g. grain low, seeds available, workbench in range).
///
/// Single source of truth consulted by both the chief's job-posting code
/// (`faction_chief_post_system`) and the workforce-budget softmax
/// (`compute_workforce_budget`), so the two cannot drift apart.
pub fn faction_can_perform(faction: &FactionData, kind: JobKind) -> bool {
    match kind {
        JobKind::Stockpile => true,
        JobKind::Haul => true,
        JobKind::Farm => faction.techs.has(CROP_CULTIVATION),
        JobKind::Build => true,
        JobKind::Craft => true,
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
    pub min: (i32, i32),
    pub max: (i32, i32),
}

impl TileAabb {
    pub fn contains(&self, tile: (i32, i32)) -> bool {
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
    /// Stockpile food: total calories deposited at faction storage. Posted
    /// against `JobKind::Stockpile`.
    Calories { deposited: u32, target: u32 },
    /// Stockpile a specific construction material (Wood, Stone, ...) into
    /// faction storage: item-count target. Posted against `JobKind::Stockpile`.
    /// Targets blend anticipatory reserves with active blueprint demand.
    Stockpile {
        resource_id: crate::economy::resource_catalog::ResourceId,
        deposited: u32,
        target: u32,
    },
    /// Haul a resource from faction storage into a specific blueprint's deposit
    /// slot. Posted against `JobKind::Haul` once storage covers (some of) the
    /// blueprint's demand. `delivered`/`target` are item counts.
    Haul {
        blueprint: Entity,
        resource_id: crate::economy::resource_catalog::ResourceId,
        delivered: u32,
        target: u32,
    },
    /// Farm: tiles successfully planted within the designated area.
    Planting {
        planted: u32,
        target: u32,
        area: TileAabb,
    },
    /// Craft: units of a specific recipe produced. `tech_payload` is set when
    /// the recipe is Clay Tablet / Book — it travels through the spawned
    /// `CraftOrder` and ends up on the produced `Item`. `None` for every
    /// non-knowledge recipe.
    Crafting {
        crafted: u32,
        target: u32,
        recipe: RecipeId,
        bench: Option<Entity>,
        tech_payload: Option<crate::simulation::technology::TechId>,
    },
    /// Build: completes when the named blueprint entity despawns.
    Building { blueprint: Entity },
}

impl JobProgress {
    pub fn is_complete(&self) -> bool {
        match self {
            JobProgress::Calories { deposited, target } => deposited >= target,
            JobProgress::Stockpile {
                deposited, target, ..
            } => deposited >= target,
            JobProgress::Haul {
                delivered, target, ..
            } => delivered >= target,
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
            JobProgress::Stockpile {
                deposited, target, ..
            } => {
                if *target == 0 {
                    1.0
                } else {
                    (*deposited as f32 / *target as f32).clamp(0.0, 1.0)
                }
            }
            JobProgress::Haul {
                delivered, target, ..
            } => {
                if *target == 0 {
                    1.0
                } else {
                    (*delivered as f32 / *target as f32).clamp(0.0, 1.0)
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

/// Pluralist Economy R6: who posted a job. The chief retains
/// today's communal-labor postings (Stockpile/Haul/Build/Craft/Farm)
/// for any resource still flagged `chief_allocates_labor=true`.
/// Bureaucrats post public-works infrastructure when
/// `state_funds_public_works=true`. Household heads and individuals
/// post family-needs / P2P contracts under capitalist policy.
///
/// `Chief` postings carry `reward = 0.0` and no settlement id —
/// today's communist labor allocation has no monetary signal. The
/// other variants always carry `reward > 0` and a sidecar `JobEscrow`
/// entity funded from the relevant treasury / wallet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosterClass {
    Chief,
    Bureaucrat,
    HouseholdHead,
    Individual,
}

impl Default for PosterClass {
    fn default() -> Self {
        PosterClass::Chief
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
    /// Pluralist Economy R6: who posted this job. Defaults to
    /// `Chief` to preserve today's behaviour at every existing
    /// posting site.
    pub poster_class: PosterClass,
    /// R6: monetary reward for completing this posting. Chief
    /// postings carry 0.0 (communal labor — no payment); other
    /// classes carry the funded amount. `0.0` is the legacy
    /// (non-paid) signal R9's `U_bid` scorer uses to fall back to
    /// the `priority + skill + bias - distance` formula.
    pub reward: f32,
    /// R6: which settlement the posting is anchored at. `None` for
    /// Chief postings (today's faction-scoped behaviour); `Some(id)`
    /// for per-poster-class postings under R6+. R7's per-settlement
    /// market lookup uses this to find the relevant market when the
    /// posting fulfils a market-driven need.
    pub settlement_id: Option<crate::simulation::settlement::SettlementId>,
}

impl JobPosting {
    /// Pluralist Economy R6 — values for the three new fields when
    /// the posting is a chief / legacy posting. Use via `..` syntax
    /// at every existing JobPosting construction site to avoid
    /// repeating three lines per call:
    ///
    /// ```ignore
    /// JobPosting {
    ///     id, faction_id, kind, progress, claimants, priority,
    ///     source, posted_tick, expiry_tick,
    ///     ..JobPosting::chief_defaults()
    /// }
    /// ```
    ///
    /// The non-R6 fields in the returned stub are placeholders;
    /// the caller's `..` syntax overrides every field they set
    /// explicitly, so only `poster_class / reward / settlement_id`
    /// are read from the stub.
    pub fn chief_defaults() -> Self {
        JobPosting {
            id: 0,
            faction_id: 0,
            kind: JobKind::Stockpile,
            // Using `Calories` (the simplest progress variant) so
            // `chief_defaults()` doesn't depend on the resource
            // catalog being installed at call time. Any caller using
            // this stub should override `progress` explicitly.
            progress: JobProgress::Calories { deposited: 0, target: 1 },
            claimants: Vec::new(),
            priority: 0,
            source: JobSource::Chief,
            posted_tick: 0,
            expiry_tick: None,
            poster_class: PosterClass::Chief,
            reward: 0.0,
            settlement_id: None,
        }
    }
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

/// Companion component to `JobClaim` carrying the concrete target of the
/// currently held posting. Populated/refreshed by `job_goal_lock_system` so
/// plan resolvers can route to the claimed blueprint or resource without
/// re-querying the `JobBoard`. `None` fields mean the claim's posting kind
/// doesn't carry that target (e.g. food Stockpile claims have no blueprint).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ClaimTarget {
    pub blueprint: Option<Entity>,
    pub resource_id: Option<crate::economy::resource_catalog::ResourceId>,
}

/// Pluralist Economy R2: an escrow record attached to a sidecar
/// entity for each funded posting. The producer of the posting (a
/// household-head, bureaucrat, or wealthy individual under R6+) debits
/// `amount` from their wallet at posting time, the sidecar entity is
/// spawned with this component, and:
///
/// - On successful job completion: a `pay()` call (R5+ poster paths)
///   credits the worker, then the sidecar is despawned with
///   `amount = 0.0` so the `on_remove` hook is a no-op.
/// - On cancellation / expiry: the sidecar is despawned with the
///   original amount intact; the `on_job_escrow_remove` hook refunds
///   `amount` to `beneficiary`.
///
/// All 25 existing `aq.cancel()` sites stay untouched: cancellation
/// still happens by removing the `JobClaim` from the worker; the
/// posting cleanup that follows then despawns this sidecar, which
/// refunds via the hook. No per-cancel-site refund logic anywhere.
#[derive(Component, Clone, Copy, Debug)]
pub struct JobEscrow {
    pub amount: f32,
    pub beneficiary: Entity,
}

pub fn on_job_escrow_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(escrow) = world.get::<JobEscrow>(entity).copied() else {
        return;
    };
    if !(escrow.amount > 0.0) {
        // Cleared on successful payout; nothing to refund.
        return;
    }
    if let Some(mut econ) = world
        .get_mut::<crate::economy::agent::EconomicAgent>(escrow.beneficiary)
    {
        econ.currency += escrow.amount;
    }
    // Beneficiary may have despawned (e.g. employer died mid-job).
    // In that case the escrowed currency is lost — same semantics as
    // GroundItems on chunk unload. The system-wide invariant snapshots
    // capture this drift.
}

/// Sum of `amount` across every live `JobEscrow` in the world. R2
/// extends `CurrencySnapshot` with this term so the system-wide
/// invariant accounts for funds-in-flight.
pub fn total_escrowed_currency(world: &mut World) -> f32 {
    let mut q = world.query::<&JobEscrow>();
    q.iter(world).map(|e| e.amount).sum()
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
        // Pluralist Economy R2: refund hook for escrowed postings.
        // Mirrors `world::spatial::Indexed`'s on_remove pattern: the
        // hook fires for `despawn` / `despawn_recursive` / explicit
        // component removal, so every teardown path refunds without
        // touching the 25 existing `aq.cancel()` sites.
        app.world_mut()
            .register_component_hooks::<JobEscrow>()
            .on_remove(on_job_escrow_remove);
        app.insert_resource(JobBoard::default())
            .add_event::<JobBoardCommand>()
            .add_event::<JobCompletedEvent>();
    }
}

/// How often the chief reconciles the job board, in fixed-update ticks.
const CHIEF_POSTING_INTERVAL: u64 = 60;

/// Player postings use a high static priority so manual overrides win against
/// chief-posted jobs. Chief priority is no longer constant — see
/// `compute_priority` in `crate::simulation::projects`.
pub const PLAYER_PRIORITY: u8 = PRIORITY_PLAYER;

/// Gather posting threshold: post when `food_total / member_count` falls
/// below this value (in `ResourceId::nutrition` units, which are per-stack).
const GATHER_TARGET_PER_HEAD: u32 = 8;

/// Maximum target size for any single Gather posting, so progress is visible
/// and partial completions release some workers earlier.
const GATHER_TARGET_CAP: u32 = 600;
const GATHER_TARGET_MIN: u32 = 80;

/// Item-count clamps for material (Wood/Stone) Gather postings.
const MATERIAL_GATHER_MIN: u32 = 4;
const MATERIAL_GATHER_CAP: u32 = 32;

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
    loom_map: Res<crate::simulation::construction::LoomMap>,
    co_map: Res<crate::simulation::crafting::CraftOrderMap>,
    co_query: Query<&crate::simulation::crafting::CraftOrder>,
    projects: Res<Projects>,
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
        //    Build postings whose project is not in the Build phase are also
        //    dropped here — the chief re-posts them once materials are in.
        //    Haul postings whose target blueprint despawned are dropped.
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
                    JobProgress::Building { blueprint } => {
                        if bp_query.get(blueprint).is_err() {
                            return false;
                        }
                        match projects.for_blueprint(blueprint) {
                            Some(project) => project.phase == ProjectPhase::Build,
                            None => false,
                        }
                    }
                    JobProgress::Haul { blueprint, .. } => {
                        bp_query.get(blueprint).is_ok()
                    }
                    JobProgress::Calories { .. }
                    | JobProgress::Stockpile { .. }
                    | JobProgress::Planting { .. }
                    | JobProgress::Crafting { .. } => false,
                }
            });
        }

        // 1b. Refresh priorities on all chief-source postings still alive so
        //     they track changing faction state without us having to drop and
        //     re-post.
        for p in board.faction_postings_mut(faction_id).iter_mut() {
            if matches!(p.source, JobSource::Chief) {
                p.priority = compute_priority(faction, faction_id, p.kind, &p.progress, &projects);
            }
        }

        // 2. Build postings — only for blueprints whose project has advanced
        //    to Build phase (deposits filled). Suppressed during GatherMaterials.
        // Pluralist Economy R6-c: when `state_funds_public_works`
        // is true, the bureaucrat is the public-works poster
        // (R10+); the chief steps back. Default factions
        // (`state_funds_public_works=false`) keep chief-posting
        // Builds, preserving today's behaviour.
        let needed_builds: Vec<Entity> = if faction.state_funds_public_works {
            Vec::new()
        } else {
            let postings = board.faction_postings_mut(faction_id);
            live_bps
                .iter()
                .copied()
                .filter(|bp_entity| {
                    let in_build_phase = matches!(
                        projects.for_blueprint(*bp_entity).map(|p| p.phase),
                        Some(ProjectPhase::Build)
                    );
                    if !in_build_phase {
                        return false;
                    }
                    !postings.iter().any(|p| matches!(
                        p.progress,
                        JobProgress::Building { blueprint } if blueprint == *bp_entity
                    ))
                })
                .collect()
        };
        for bp_entity in needed_builds {
            let id = board.alloc_id();
            let progress = JobProgress::Building {
                blueprint: bp_entity,
            };
            let priority = compute_priority(faction, faction_id, JobKind::Build, &progress, &projects);
            board.faction_postings_mut(faction_id).push(JobPosting {
                id,
                faction_id,
                kind: JobKind::Build,
                progress,
                claimants: Vec::new(),
                priority,
                source: JobSource::Chief,
                posted_tick,
                expiry_tick: None,
                poster_class: crate::simulation::jobs::PosterClass::Chief,
                reward: 0.0,
                settlement_id: None,
            });
        }

        // 3. Stockpile (food) posting — one if storage food per-head is below threshold.
        // Pluralist Economy R6-a: gate on the chief's food policy.
        // If the faction has flipped Fruit (representative food) to
        // `chief_allocates_labor=false`, the chief no longer posts
        // communal food drives — private hunters/farmers sell food
        // at the regional market instead (R7+). Skipping this branch
        // is what gives capitalist factions their distinct labor
        // structure.
        let food_chief_allocates = faction
            .policy_for(crate::economy::core_ids::fruit())
            .chief_allocates_labor;
        if faction.member_count > 0 && food_chief_allocates {
            let already_food = board
                .faction_postings(faction_id)
                .iter()
                .any(|p| matches!(p.progress, JobProgress::Calories { .. }));
            if !already_food {
                let food_total = faction.storage.food_total() as u32;
                let target_supply = faction.member_count * GATHER_TARGET_PER_HEAD * 8;
                if food_total < target_supply {
                    let deficit_units = target_supply.saturating_sub(food_total);
                    // Convert deficit units to a calorie target (use Fruit nutrition
                    // as a conservative average; deposits contribute their actual
                    // good's nutrition).
                    let calories = deficit_units
                        * crate::economy::core_ids::Fruit
                            .get()
                            .copied()
                            .unwrap()
                            .nutrition() as u32;
                    let target = calories.clamp(GATHER_TARGET_MIN, GATHER_TARGET_CAP);
                    let id = board.alloc_id();
                    let progress = JobProgress::Calories {
                        deposited: 0,
                        target,
                    };
                    let priority =
                        compute_priority(faction, faction_id, JobKind::Stockpile, &progress, &projects);
                    board.faction_postings_mut(faction_id).push(JobPosting {
                        id,
                        faction_id,
                        kind: JobKind::Stockpile,
                        progress,
                        claimants: Vec::new(),
                        priority,
                        source: JobSource::Chief,
                        posted_tick,
                        expiry_tick: None,
                        poster_class: crate::simulation::jobs::PosterClass::Chief,
                        reward: 0.0,
                        settlement_id: None,
                    });
                }
            }
        }

        // 3b. Stockpile (material) postings — anticipatory + reactive. Target
        //     for each tracked Good is `max(faction.material_targets, Σ unmet
        //     across active blueprints)`. Posted whenever current storage is
        //     below target. One posting per (faction, good).
        // Pluralist Economy R6-a: gate per-resource on
        // `chief_allocates_labor`. Capitalist factions skip this
        // branch for any flipped resource — private actors handle
        // it via market trade.
        if faction.member_count > 0 {
            let wood_id = crate::economy::core_ids::wood();
            let stone_id = crate::economy::core_ids::stone();
            for &target_rid in &[wood_id, stone_id] {
                if !faction.policy_for(target_rid).chief_allocates_labor {
                    continue;
                }
                // Sum unmet blueprint demand for this resource (reactive component).
                let mut bp_demand: u32 = 0;
                for &bp_entity in &live_bps {
                    let Ok(bp) = bp_query.get(bp_entity) else {
                        continue;
                    };
                    for slot in &bp.deposits[..bp.deposit_count as usize] {
                        if slot.resource_id == target_rid {
                            bp_demand = bp_demand
                                .saturating_add((slot.needed.saturating_sub(slot.deposited)) as u32);
                        }
                    }
                }
                let anticipatory = faction.material_target_of(target_rid);
                let target_total = anticipatory.max(bp_demand);
                if target_total == 0 {
                    continue;
                }
                let stored = faction.storage.stock_of(target_rid);
                if stored >= target_total {
                    continue;
                }
                let deficit = target_total.saturating_sub(stored);
                let already = board.faction_postings(faction_id).iter().any(|p| {
                    matches!(
                        &p.progress,
                        JobProgress::Stockpile { resource_id, .. } if *resource_id == target_rid
                    )
                });
                if already {
                    continue;
                }
                let target = deficit.clamp(MATERIAL_GATHER_MIN, MATERIAL_GATHER_CAP);
                let id = board.alloc_id();
                let progress = JobProgress::Stockpile {
                    resource_id: target_rid,
                    deposited: 0,
                    target,
                };
                let priority =
                    compute_priority(faction, faction_id, JobKind::Stockpile, &progress, &projects);
                board.faction_postings_mut(faction_id).push(JobPosting {
                    id,
                    faction_id,
                    kind: JobKind::Stockpile,
                    progress,
                    claimants: Vec::new(),
                    priority,
                    source: JobSource::Chief,
                    posted_tick,
                    expiry_tick: None,
                    poster_class: crate::simulation::jobs::PosterClass::Chief,
                    reward: 0.0,
                    settlement_id: None,
                });
            }
        }

        // 3b-ii. Phase 5e-xiv: Stockpile postings driven by open `CraftOrder`
        //        demand for any resource not already covered by 3b's
        //        Wood/Stone iteration. Replaces the legacy `DeliverHide` plan
        //        (PlanId 13) for the Skin path: when a faction CraftOrder
        //        needs Skin (Bow / Leather Armor) and storage doesn't yet
        //        have it, the chief posts `Stockpile { Skin }` and a worker
        //        scavenges ambient hide drops (from butchery at the hearth)
        //        into storage. The existing Haul posting (3c) then fires
        //        once storage has stock.
        if faction.member_count > 0 {
            // Aggregate per-resource still-needed across all open faction
            // CraftOrders. Only process resources NOT already handled by 3b.
            let wood_id = crate::economy::core_ids::wood();
            let stone_id = crate::economy::core_ids::stone();
            let mut co_demand: AHashMap<
                crate::economy::resource_catalog::ResourceId,
                u32,
            > = AHashMap::new();
            for (_, &order_entity) in &co_map.0 {
                let Ok(order) = co_query.get(order_entity) else {
                    continue;
                };
                if order.faction_id != faction_id {
                    continue;
                }
                for slot in &order.deposits[..order.deposit_count as usize] {
                    if slot.resource_id == wood_id || slot.resource_id == stone_id {
                        continue;
                    }
                    let still = slot.needed.saturating_sub(slot.deposited) as u32;
                    if still == 0 {
                        continue;
                    }
                    *co_demand.entry(slot.resource_id).or_insert(0) =
                        co_demand
                            .get(&slot.resource_id)
                            .copied()
                            .unwrap_or(0)
                            .saturating_add(still);
                }
            }
            for (target_rid, demand) in co_demand {
                if demand == 0 {
                    continue;
                }
                // R6-a: skip when the resource is privately handled.
                if !faction.policy_for(target_rid).chief_allocates_labor {
                    continue;
                }
                let stored = faction.storage.stock_of(target_rid);
                if stored >= demand {
                    continue;
                }
                let deficit = demand.saturating_sub(stored);
                let already = board.faction_postings(faction_id).iter().any(|p| {
                    matches!(
                        &p.progress,
                        JobProgress::Stockpile { resource_id, .. } if *resource_id == target_rid
                    )
                });
                if already {
                    continue;
                }
                let target = deficit.clamp(MATERIAL_GATHER_MIN, MATERIAL_GATHER_CAP);
                let id = board.alloc_id();
                let progress = JobProgress::Stockpile {
                    resource_id: target_rid,
                    deposited: 0,
                    target,
                };
                let priority =
                    compute_priority(faction, faction_id, JobKind::Stockpile, &progress, &projects);
                board.faction_postings_mut(faction_id).push(JobPosting {
                    id,
                    faction_id,
                    kind: JobKind::Stockpile,
                    progress,
                    claimants: Vec::new(),
                    priority,
                    source: JobSource::Chief,
                    posted_tick,
                    expiry_tick: None,
                    poster_class: crate::simulation::jobs::PosterClass::Chief,
                    reward: 0.0,
                    settlement_id: None,
                });
            }
        }

        // 3c. Haul postings — per-blueprint, per-good. Posted only when
        //     faction storage covers (some of) the blueprint's demand. The
        //     hauler withdraws from storage and deposits into the blueprint.
        //     Storage availability is shared across blueprints: the chief
        //     allocates greedily by blueprint iteration order.
        if faction.member_count > 0 {
            // Phase 2d: storage_remaining, deposit slots, and the Haul
            // JobProgress payload are all ResourceId-keyed end-to-end —
            // no Good roundtrip on this hot path.
            let mut storage_remaining: AHashMap<
                crate::economy::resource_catalog::ResourceId,
                u32,
            > = faction.storage.totals.clone();
            // Subtract qty already committed to existing alive Haul postings
            // (not yet delivered) so we don't double-allocate the same stock.
            for p in board.faction_postings(faction_id).iter() {
                if let JobProgress::Haul {
                    resource_id,
                    delivered,
                    target,
                    ..
                } = &p.progress
                {
                    let outstanding = target.saturating_sub(*delivered);
                    let entry = storage_remaining.entry(*resource_id).or_insert(0);
                    *entry = entry.saturating_sub(outstanding);
                }
            }
            for &bp_entity in &live_bps {
                let Ok(bp) = bp_query.get(bp_entity) else {
                    continue;
                };
                for slot in &bp.deposits[..bp.deposit_count as usize] {
                    let remaining = slot.needed.saturating_sub(slot.deposited) as u32;
                    if remaining == 0 {
                        continue;
                    }
                    // R6-b: skip when the resource is privately
                    // allocated. Capitalist hauls are organised by
                    // household-heads / individuals (R10+), not the
                    // chief.
                    if !faction.policy_for(slot.resource_id).chief_allocates_labor {
                        continue;
                    }
                    // Already a live Haul posting for (this BP, this resource)?
                    let already = board.faction_postings(faction_id).iter().any(|p| {
                        matches!(
                            &p.progress,
                            JobProgress::Haul { blueprint: b, resource_id: r, .. }
                                if *b == bp_entity && *r == slot.resource_id
                        )
                    });
                    if already {
                        continue;
                    }
                    let slot_id = slot.resource_id;
                    let avail = storage_remaining.get(&slot_id).copied().unwrap_or(0);
                    if avail == 0 {
                        continue;
                    }
                    let target = remaining.min(avail);
                    if target == 0 {
                        continue;
                    }
                    let entry = storage_remaining.entry(slot_id).or_insert(0);
                    *entry = entry.saturating_sub(target);
                    let id = board.alloc_id();
                    let progress = JobProgress::Haul {
                        blueprint: bp_entity,
                        resource_id: slot_id,
                        delivered: 0,
                        target,
                    };
                    let priority =
                        compute_priority(faction, faction_id, JobKind::Haul, &progress, &projects);
                    board.faction_postings_mut(faction_id).push(JobPosting {
                        id,
                        faction_id,
                        kind: JobKind::Haul,
                        progress,
                        claimants: Vec::new(),
                        priority,
                        source: JobSource::Chief,
                        posted_tick,
                        expiry_tick: None,
                        poster_class: crate::simulation::jobs::PosterClass::Chief,
                        reward: 0.0,
                        settlement_id: None,
                    });
                }
            }
        }

        // 4. Farm posting — capability gated (Agriculture / CROP_CULTIVATION).
        // Pluralist Economy R6-e: gate on Grain's policy. When the
        // chief has flipped Grain to `chief_allocates_labor=false`,
        // private farmers handle planting and selling at the
        // regional market; no chief Farm posting.
        let farm_chief_allocates = faction
            .policy_for(crate::economy::core_ids::grain())
            .chief_allocates_labor;
        if farm_chief_allocates
            && faction_can_perform(faction, JobKind::Farm)
            && faction.member_count > 0
        {
            let already_farm = board
                .faction_postings(faction_id)
                .iter()
                .any(|p| matches!(p.kind, JobKind::Farm));
            let grain = faction
                .storage
                .stock_of(crate::economy::core_ids::grain());
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
                let progress = JobProgress::Planting {
                    planted: 0,
                    target: FARM_TILES_PER_POST.min(seed),
                    area,
                };
                let priority =
                    compute_priority(faction, faction_id, JobKind::Farm, &progress, &projects);
                board.faction_postings_mut(faction_id).push(JobPosting {
                    id,
                    faction_id,
                    kind: JobKind::Farm,
                    progress,
                    claimants: Vec::new(),
                    priority,
                    source: JobSource::Chief,
                    posted_tick,
                    expiry_tick: None,
                    poster_class: crate::simulation::jobs::PosterClass::Chief,
                    reward: 0.0,
                    settlement_id: None,
                });
            }
        }

        // 5. Craft posting — pick the available recipe with the largest
        // unmet demand on its output good. One Craft posting per faction at a
        // time; subsequent cycles rotate through recipes as deficits shift.
        if faction.member_count > 0 {
            let already_craft = board
                .faction_postings(faction_id)
                .iter()
                .any(|p| matches!(p.kind, JobKind::Craft));
            if !already_craft {
                let in_home_zone = |tile: &(i32, i32)| {
                    let dx = (tile.0 as i32 - faction.home_tile.0 as i32).abs();
                    let dy = (tile.1 as i32 - faction.home_tile.1 as i32).abs();
                    dx <= 12 && dy <= 12
                };
                let bench: Option<Entity> = workbench_map
                    .0
                    .iter()
                    .find(|(t, _)| in_home_zone(t))
                    .map(|(_, e)| *e);
                let loom: Option<Entity> = loom_map
                    .0
                    .iter()
                    .find(|(t, _)| in_home_zone(t))
                    .map(|(_, e)| *e);

                let mut best: Option<(u8, u32, Option<Entity>)> = None;
                for (idx, recipe) in crate::simulation::crafting::craft_recipes().iter().enumerate() {
                    if let Some(tech) = recipe.tech_gate {
                        if !faction.techs.has(tech) {
                            continue;
                        }
                    }
                    // R6-d: skip recipes whose output resource is
                    // privately allocated. Capitalist factions let
                    // smiths self-direct toward profitable recipes
                    // (R10+) or fulfil P2P contracts (R12).
                    if !faction
                        .policy_for(recipe.output_resource)
                        .chief_allocates_labor
                    {
                        continue;
                    }
                    let bench_ref = match recipe.requires_station {
                        Some(crate::simulation::crafting::StationKind::Workbench) => {
                            match bench {
                                Some(e) => Some(e),
                                None => continue,
                            }
                        }
                        Some(crate::simulation::crafting::StationKind::Loom) => {
                            if loom.is_none() {
                                continue;
                            }
                            // `job_claim_release_system` only validates Workbench
                            // entities; pass None for looms so the release sweep
                            // doesn't drop the posting.
                            None
                        }
                        None => None,
                    };
                    // Phase 2d: resource_supply/demand are ResourceId-keyed,
                    // so we use the recipe's output_resource directly.
                    let supply = faction
                        .resource_supply
                        .get(&recipe.output_resource)
                        .copied()
                        .unwrap_or(0);
                    let demand = faction
                        .resource_demand
                        .get(&recipe.output_resource)
                        .copied()
                        .unwrap_or(0);
                    if demand <= supply {
                        continue;
                    }
                    let deficit = demand - supply;
                    // Only post when ingredients are actually available;
                    // otherwise workers adopt Craft goal with no CraftOrder.
                    let mut has_ingredients = true;
                    for &(id, qty) in recipe.inputs.iter() {
                        if faction.resource_supply.get(&id).copied().unwrap_or(0) < qty {
                            has_ingredients = false;
                            break;
                        }
                    }
                    if !has_ingredients {
                        continue;
                    }
                    if best.map_or(true, |(_, d, _)| deficit > d) {
                        best = Some((idx as u8, deficit, bench_ref));
                    }
                }

                if let Some((recipe_id, deficit, bench_ref)) = best {
                    let target = deficit.min(5);
                    let id = board.alloc_id();
                    let progress = JobProgress::Crafting {
                        crafted: 0,
                        target,
                        recipe: recipe_id,
                        bench: bench_ref,
                        // Demand-driven crafts (Spear, Cloth, Iron Tools, ...)
                        // do not encode tech. The tablet/book pipeline posts
                        // separately via `chief_tablet_posting_system`.
                        tech_payload: None,
                    };
                    let priority =
                        compute_priority(faction, faction_id, JobKind::Craft, &progress, &projects);
                    board.faction_postings_mut(faction_id).push(JobPosting {
                        id,
                        faction_id,
                        kind: JobKind::Craft,
                        progress,
                        claimants: Vec::new(),
                        priority,
                        source: JobSource::Chief,
                        posted_tick,
                        expiry_tick: None,
                        poster_class: crate::simulation::jobs::PosterClass::Chief,
                        reward: 0.0,
                        settlement_id: None,
                    });
                }
            }
        }
    }
}

/// Player override for crafting a specific tablet/book on demand. Inspector
/// "Encode Tablet" writes the recipe + tech_payload here; the apply system
/// posts a Craft job to the player faction at the next chief-posting tick.
#[derive(Resource, Default)]
pub struct PlayerCraftRequest(pub Option<(u8, Option<crate::simulation::technology::TechId>)>);

/// Cadence: chief reconsiders tablet posting once per game-day. See plan
/// memory `feedback_game_time_pacing.md` — faction-level decisions anchor on
/// game time, not 60-tick reactive cadence.
const CHIEF_TABLET_POSTING_INTERVAL: u64 = 3600;

/// Auto-post Clay Tablet craft jobs when the chief has Learned a tech the
/// rest of the faction is largely unaware of. Runs once per game-day per
/// faction; player override via `PlayerCraftRequest` is consumed every tick.
pub fn chief_tablet_posting_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    workbench_map: Res<crate::simulation::construction::WorkbenchMap>,
    mut player_request: ResMut<PlayerCraftRequest>,
    player: Res<crate::simulation::faction::PlayerFaction>,
    mut board: ResMut<JobBoard>,
    knowledge_query: Query<&crate::simulation::knowledge::PersonKnowledge>,
    members_query: Query<(
        &FactionMember,
        &crate::simulation::knowledge::PersonKnowledge,
        &crate::simulation::stats::Stats,
        &LodLevel,
    )>,
) {
    use crate::simulation::crafting::{craft_recipes, recipe_encodes_knowledge, RECIPE_CLAY_TABLET};
    use crate::simulation::technology::{complexity, TechId, TECH_COUNT};

    let posted_tick = clock.tick as u32;

    // Fast path: consume player override (always, regardless of cadence).
    if let Some((recipe_id, tech_payload)) = player_request.0.take() {
        if recipe_encodes_knowledge(recipe_id) {
            let faction_id = player.faction_id;
            if let Some(faction) = registry.factions.get(&faction_id) {
                let in_home_zone = |tile: &(i32, i32)| {
                    let dx = (tile.0 as i32 - faction.home_tile.0 as i32).abs();
                    let dy = (tile.1 as i32 - faction.home_tile.1 as i32).abs();
                    dx <= 16 && dy <= 16
                };
                let bench_ok = workbench_map.0.iter().any(|(t, _)| in_home_zone(t));
                let dup = board.faction_postings(faction_id).iter().any(|p| {
                    matches!(p.progress,
                        JobProgress::Crafting { recipe, tech_payload: tp, .. }
                            if recipe == recipe_id && tp == tech_payload)
                });
                if bench_ok && !dup {
                    let id = board.alloc_id();
                    let progress = JobProgress::Crafting {
                        crafted: 0,
                        target: 1,
                        recipe: recipe_id,
                        bench: None,
                        tech_payload,
                    };
                    let priority = 180; // High — the player asked for it.
                    board.faction_postings_mut(faction_id).push(JobPosting {
                        id,
                        faction_id,
                        kind: JobKind::Craft,
                        progress,
                        claimants: Vec::new(),
                        priority,
                        source: JobSource::Player,
                        posted_tick,
                        expiry_tick: None,
                        poster_class: crate::simulation::jobs::PosterClass::Chief,
                        reward: 0.0,
                        settlement_id: None,
                    });
                }
            }
        }
    }

    // Slow path: chief autonomous tablet posting.
    if clock.tick % CHIEF_TABLET_POSTING_INTERVAL != 0 {
        return;
    }

    for (&faction_id, faction) in registry.factions.iter() {
        if faction_id == SOLO {
            continue;
        }
        // Workbench in zone.
        let in_home_zone = |tile: &(i32, i32)| {
            let dx = (tile.0 as i32 - faction.home_tile.0 as i32).abs();
            let dy = (tile.1 as i32 - faction.home_tile.1 as i32).abs();
            dx <= 16 && dy <= 16
        };
        let has_bench = workbench_map.0.iter().any(|(t, _)| in_home_zone(t));
        if !has_bench {
            continue;
        }
        // Need the chief's bitsets.
        let Some(chief_e) = faction.chief_entity else {
            continue;
        };
        let Ok(chief_knowledge) = knowledge_query.get(chief_e) else {
            continue;
        };
        if chief_knowledge.learned == 0 {
            continue;
        }

        // Tally adult awareness across faction members.
        let mut adults = 0u32;
        let mut aware_count = [0u32; 64];
        for (m, k, stats, lod) in members_query.iter() {
            if m.faction_id != faction_id || *lod == LodLevel::Dormant {
                continue;
            }
            // Treat anyone with stats present as adult-eligible. (The repo's
            // adult predicate lives elsewhere — use member count as a proxy
            // and let the awareness threshold do the gating.)
            let _ = stats;
            adults += 1;
            for id in 0..TECH_COUNT as TechId {
                if k.is_aware(id) {
                    aware_count[id as usize] += 1;
                }
            }
        }
        if adults < 2 {
            continue;
        }
        let half = adults / 2;

        // Pick the highest-complexity tech the chief Learned that is lacking
        // awareness in the faction. Skip techs already encoded by a live
        // tablet posting/order.
        let live_tablet_techs: Vec<TechId> = board
            .faction_postings(faction_id)
            .iter()
            .filter_map(|p| match p.progress {
                JobProgress::Crafting {
                    recipe,
                    tech_payload: Some(t),
                    ..
                } if recipe_encodes_knowledge(recipe) => Some(t),
                _ => None,
            })
            .collect();

        let mut chosen: Option<(TechId, u8)> = None;
        for id in 0..TECH_COUNT as TechId {
            if !chief_knowledge.has_learned(id) {
                continue;
            }
            if aware_count[id as usize] >= half {
                continue;
            }
            if live_tablet_techs.contains(&id) {
                continue;
            }
            let cx = complexity(id);
            match chosen {
                None => chosen = Some((id, cx)),
                Some((_, best_cx)) if cx > best_cx => chosen = Some((id, cx)),
                _ => {}
            }
        }

        let Some((tech, _cx)) = chosen else {
            continue;
        };

        // Verify recipe ingredients are available (Stone+Wood for tablet).
        let recipe = &craft_recipes()[RECIPE_CLAY_TABLET as usize];
        let mut ok = true;
        for &(id, qty) in recipe.inputs.iter() {
            // Phase 2d: resource_supply is ResourceId-keyed.
            if faction.resource_supply.get(&id).copied().unwrap_or(0) < qty {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }

        let id = board.alloc_id();
        let progress = JobProgress::Crafting {
            crafted: 0,
            target: 1,
            recipe: RECIPE_CLAY_TABLET,
            bench: None,
            tech_payload: Some(tech),
        };
        board.faction_postings_mut(faction_id).push(JobPosting {
            id,
            faction_id,
            kind: JobKind::Craft,
            progress,
            claimants: Vec::new(),
            priority: 100,
            source: JobSource::Chief,
            posted_tick,
            expiry_tick: None,
            poster_class: crate::simulation::jobs::PosterClass::Chief,
            reward: 0.0,
            settlement_id: None,
        });
    }
}

/// Skill axis used when scoring a candidate job for a worker.
fn skill_for(kind: JobKind) -> SkillKind {
    match kind {
        JobKind::Stockpile => SkillKind::Farming,
        JobKind::Haul => SkillKind::Farming,
        JobKind::Farm => SkillKind::Farming,
        JobKind::Build => SkillKind::Building,
        JobKind::Craft => SkillKind::Crafting,
    }
}

/// Personality additive bias for job kinds. Range -0.2 .. +0.4.
fn personality_bias(p: Personality, kind: JobKind) -> f32 {
    match (p, kind) {
        (Personality::Gatherer, JobKind::Stockpile) => 0.4,
        (Personality::Gatherer, JobKind::Farm) => 0.2,
        (Personality::Nurturer, JobKind::Farm) => 0.3,
        (Personality::Nurturer, JobKind::Stockpile) => 0.2,
        (Personality::Nurturer, JobKind::Craft) => 0.1,
        (Personality::Explorer, JobKind::Stockpile) => 0.2,
        (Personality::Explorer, JobKind::Farm) => -0.1,
        (Personality::Socialite, JobKind::Build) => 0.1,
        (Personality::Socialite, JobKind::Haul) => 0.15,
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
        (Profession::Farmer, JobKind::Stockpile) => 0.1,
        _ => 0.0,
    }
}

/// Distinguishing key for claim-cap accounting. Stockpile postings split by
/// the targeted resource so wood/stone caps are independent of food's. All
/// other kinds (Haul, Build, Farm, Craft) cap as a single bucket per JobKind.
fn cap_bucket(p: &JobPosting) -> (JobKind, Option<crate::economy::resource_catalog::ResourceId>) {
    match (&p.kind, &p.progress) {
        (JobKind::Stockpile, JobProgress::Stockpile { resource_id, .. }) => {
            (JobKind::Stockpile, Some(*resource_id))
        }
        (JobKind::Stockpile, JobProgress::Calories { .. }) => (JobKind::Stockpile, None),
        _ => (p.kind, None),
    }
}

/// Per-bucket workforce share. For Stockpile, dispatches to the per-resource
/// slice; otherwise the kind-level share.
fn bucket_share(
    budget: &crate::simulation::projects::WorkforceBudget,
    kind: JobKind,
    resource_id: Option<crate::economy::resource_catalog::ResourceId>,
) -> f32 {
    match (kind, resource_id) {
        (JobKind::Stockpile, Some(r)) => budget.stockpile_share(r),
        (JobKind::Stockpile, None) => budget.stockpile_food,
        _ => budget.share(kind),
    }
}

/// Claim available jobs for idle workers, with full scoring and per-bucket
/// caps. Stockpile postings are capped per-Good so a food posting can't
/// monopolise the wood/stone allotment.
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
            Option<&crate::simulation::knowledge::PersonKnowledge>,
        ),
        Without<JobClaim>,
    >,
) {
    let posted_tick = clock.tick as u32;

    // Pre-pass: count active claims per (faction_id, kind, Option<Good>) by
    // scanning the board's claimant lists. Stockpile postings split by Good so
    // food/wood/stone get independent caps.
    let mut claim_counts: AHashMap<
        (
            u32,
            JobKind,
            Option<crate::economy::resource_catalog::ResourceId>,
        ),
        u32,
    > = AHashMap::new();
    for (faction_id, postings) in board.postings.iter() {
        for p in postings.iter() {
            let (kind, rid) = cap_bucket(p);
            *claim_counts.entry((*faction_id, kind, rid)).or_insert(0) +=
                p.claimants.len() as u32;
        }
    }

    for (worker, member, ai, lod, skills, personality, profession_opt, transform, knowledge_opt) in
        workers.iter()
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
        let budget = faction.workforce_budget;
        let profession = profession_opt.copied().unwrap_or(Profession::None);
        let worker_tile = (
            (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32,
            (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32,
        );

        // Score every eligible posting and pick the best.
        let mut best: Option<(usize, f32)> = None;
        let postings = board.faction_postings(faction_id);
        for (idx, p) in postings.iter().enumerate() {
            // Cap: per-bucket allocation is the workforce budget share applied
            // to current member count, floored at 1 so even small factions
            // can keep one worker on each role. Stockpile postings cap by
            // Good so food can't crowd out wood/stone slots.
            let (kind, rid) = cap_bucket(p);
            let share = bucket_share(&budget, kind, rid);
            let cap = ((share * faction.member_count as f32).round() as u32).max(1);
            let count = claim_counts
                .get(&(faction_id, kind, rid))
                .copied()
                .unwrap_or(0);
            if count >= cap {
                continue;
            }
            // Skip postings that completed but haven't been removed yet.
            if p.progress.is_complete() {
                continue;
            }
            // Per-person craft tech-gate: a worker can only claim a Craft
            // posting whose recipe `tech_gate` they have personally Learned.
            // Faction-level posting still uses chief awareness; this filter
            // prevents low-knowledge workers from grabbing tablet/book jobs
            // they can't actually execute.
            if let JobProgress::Crafting { recipe, .. } = p.progress {
                if let Some(rdef) =
                    crate::simulation::crafting::craft_recipes().get(recipe as usize)
                {
                    if let Some(req_tech) = rdef.tech_gate {
                        let knows = knowledge_opt
                            .map(|k| k.has_learned(req_tech))
                            .unwrap_or(false);
                        if !knows {
                            continue;
                        }
                    }
                }
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
        let (kind, rid) = cap_bucket(posting);
        posting.claimants.push(worker);
        *claim_counts.entry((faction_id, kind, rid)).or_insert(0) += 1;
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
fn posting_target_tile(p: &JobPosting) -> Option<(i32, i32)> {
    match p.progress {
        JobProgress::Calories { .. } => None,
        JobProgress::Stockpile { .. } => None,
        JobProgress::Haul { .. } => None,
        JobProgress::Planting { area, .. } => Some((
            (area.min.0 as i32 + area.max.0 as i32) as i32 / 2,
            (area.min.1 as i32 + area.max.1 as i32) as i32 / 2,
        )),
        JobProgress::Crafting { .. } => None,
        JobProgress::Building { .. } => None,
    }
}

/// Map a posting to the agent goal a claimant should adopt. Stockpile postings
/// dispatch by the specific Good so wood/stone gathering uses the right plan;
/// Haul and Build postings route through their kind-level mapping.
pub fn posting_goal(p: &JobPosting) -> AgentGoal {
    use crate::economy::core_ids;
    match (&p.kind, &p.progress) {
        (JobKind::Stockpile, JobProgress::Stockpile { resource_id, .. }) => {
            let wood = core_ids::Wood.get().copied();
            let stone = core_ids::Stone.get().copied();
            if Some(*resource_id) == wood {
                AgentGoal::GatherWood
            } else if Some(*resource_id) == stone {
                AgentGoal::GatherStone
            } else {
                // Phase 5e-xiv: any non-Wood/Stone Stockpile posting maps to
                // the generalized `Stockpile` goal. The specific resource
                // travels via `ClaimTarget.resource_id` so the dispatcher
                // (`htn_acquire_good_dispatch_system`'s Stockpile branch) can
                // scavenge ambient ground items of the right kind.
                AgentGoal::Stockpile
            }
        }
        (JobKind::Stockpile, JobProgress::Calories { .. }) => AgentGoal::GatherFood,
        (JobKind::Haul, _) => AgentGoal::Haul,
        _ => p.kind.to_goal(),
    }
}

/// After `goal_update_system` has run for the tick, lock claimed workers'
/// goals to the job kind. If a crisis-class goal won (Survive/Defend/Raid/
/// Rescue), drop the claim instead — the crisis takes precedence and the
/// worker is freed from the job board.
///
/// Also refreshes the `ClaimTarget` companion component so plan resolvers
/// can route to the specific blueprint/good named in the claimed posting.
pub fn job_goal_lock_system(
    mut commands: Commands,
    mut board: ResMut<JobBoard>,
    mut workers: Query<(Entity, &mut AgentGoal, &JobClaim, Option<&mut ClaimTarget>)>,
) {
    for (worker, mut goal, claim, mut target_opt) in workers.iter_mut() {
        let crisis = matches!(
            *goal,
            AgentGoal::Survive | AgentGoal::Defend | AgentGoal::Raid | AgentGoal::Rescue
        );
        if crisis {
            commands.entity(worker).remove::<JobClaim>();
            commands.entity(worker).remove::<ClaimTarget>();
            release_claimant(&mut board, claim.job_id, worker);
            continue;
        }
        let target = if let Some(p) = board.get(claim.job_id) {
            *goal = posting_goal(p);
            posting_claim_target(p)
        } else {
            *goal = claim.kind.to_goal();
            ClaimTarget::default()
        };
        match target_opt.as_mut() {
            Some(existing) => {
                **existing = target;
            }
            None => {
                commands.entity(worker).insert(target);
            }
        }
    }
}

/// Snapshot a posting's concrete target into a `ClaimTarget`. Returns the
/// blueprint and good a hauler/builder should route to; non-targeted postings
/// (food stockpile, planting) yield `ClaimTarget::default()`.
pub fn posting_claim_target(p: &JobPosting) -> ClaimTarget {
    match &p.progress {
        JobProgress::Stockpile { resource_id, .. } => ClaimTarget {
            blueprint: None,
            resource_id: Some(*resource_id),
        },
        JobProgress::Haul {
            blueprint,
            resource_id,
            ..
        } => ClaimTarget {
            blueprint: Some(*blueprint),
            resource_id: Some(*resource_id),
        },
        JobProgress::Building { blueprint } => ClaimTarget {
            blueprint: Some(*blueprint),
            resource_id: None,
        },
        _ => ClaimTarget::default(),
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
            commands.entity(c).remove::<ClaimTarget>();
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
    record_progress_filtered(
        commands,
        board,
        completed_events,
        claim,
        kind_filter,
        None,
        increment,
    );
}

/// Variant of `record_progress` that also gates on the deposited `ResourceId`.
/// Used by the deposit hook to credit `JobProgress::Stockpile`/`Haul` postings
/// only when the worker drops the matching resource. Pass `resource_id=None`
/// for callers that don't care about resource matching (food calorie credits).
pub fn record_progress_filtered(
    commands: &mut Commands,
    board: &mut JobBoard,
    completed_events: &mut EventWriter<JobCompletedEvent>,
    claim: &JobClaim,
    kind_filter: JobKind,
    resource_id: Option<crate::economy::resource_catalog::ResourceId>,
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
            // Calorie credits only apply when caller didn't request a specific
            // material (i.e. food deposits).
            if resource_id.is_some() {
                return;
            }
            *deposited = deposited.saturating_add(increment);
            if deposited >= target {
                completed = true;
            }
        }
        JobProgress::Stockpile {
            resource_id: posting_rid,
            deposited,
            target,
        } => {
            // Only credit if the caller is depositing the matching resource.
            if resource_id != Some(*posting_rid) {
                return;
            }
            *deposited = deposited.saturating_add(increment);
            if deposited >= target {
                completed = true;
            }
        }
        JobProgress::Haul {
            resource_id: posting_rid,
            delivered,
            target,
            ..
        } => {
            // Only credit if the caller is depositing the matching resource.
            if resource_id != Some(*posting_rid) {
                return;
            }
            *delivered = delivered.saturating_add(increment);
            if delivered >= target {
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
            commands.entity(c).remove::<ClaimTarget>();
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
pub fn planting_area_contains(progress: &JobProgress, tile: (i32, i32)) -> bool {
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
                        commands.entity(c).remove::<ClaimTarget>();
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
/// the same blueprint, recipe, farm area, or stockpiled good. Calorie postings
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
        (
            JobProgress::Stockpile { resource_id: rx, .. },
            JobProgress::Stockpile { resource_id: ry, .. },
        ) => rx == ry,
        (
            JobProgress::Haul {
                blueprint: bx,
                resource_id: rx,
                ..
            },
            JobProgress::Haul {
                blueprint: by,
                resource_id: ry,
                ..
            },
        ) => bx == by && rx == ry,
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
                commands.entity(c).remove::<ClaimTarget>();
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
            commands.entity(worker).remove::<ClaimTarget>();
            release_claimant(&mut board, job_id, worker);
        }
    }
}
