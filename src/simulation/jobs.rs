use ahash::AHashMap;
use bevy::ecs::entity::Entities;
use bevy::prelude::*;

use crate::simulation::construction::{Blueprint, BlueprintMap};
use crate::simulation::faction::{FactionData, FactionMember, FactionRegistry, SOLO};
use crate::simulation::goals::{is_maintenance_goal, AgentGoal, Personality};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{PersonAI, Profession, UNEMPLOYED_TASK_KIND};
use crate::simulation::projects::{
    compute_priority, food_pressure, ProjectPhase, Projects, CRITICAL_FOOD_TRIGGER, PRIORITY_PLAYER,
};
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
    /// Draftwork v2: walk an Agricultural plot tile-by-tile with a draft
    /// animal (Cattle / Horse), turning unplowed soil over so the year's
    /// crop carries the `Tilled` marker for a 1.4× harvest bonus. Posted
    /// by the chief in Spring per un-plowed plot when the faction has
    /// `ARD_PLOW` tech + an `ard_plow` implement in storage + a trained
    /// animal. Maps to `AgentGoal::Farm` (piggybacks on the Farm goal).
    Plow,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Stockpile => "Stockpile",
            JobKind::Haul => "Haul",
            JobKind::Farm => "Farm",
            JobKind::Craft => "Craft",
            JobKind::Build => "Build",
            JobKind::Plow => "Plow",
        }
    }

    pub fn to_goal(self) -> AgentGoal {
        match self {
            JobKind::Stockpile => AgentGoal::GatherFood,
            JobKind::Haul => AgentGoal::Haul,
            JobKind::Farm => AgentGoal::Farm,
            JobKind::Craft => AgentGoal::Craft,
            JobKind::Build => AgentGoal::Build,
            // Plow piggybacks on the Farm goal — workers with `AgentGoal::Farm`
            // pick up Plow claims via the standard claim flow.
            JobKind::Plow => AgentGoal::Farm,
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
        // Plow requires the ard plow tech AND grants `AgentGoal::Farm`, so
        // gate on both: tech + grain-policy chief_allocates (same as Farm).
        JobKind::Plow => faction.techs.has(crate::simulation::technology::ARD_PLOW),
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
        tile.0 >= self.min.0 && tile.0 <= self.max.0 && tile.1 >= self.min.1 && tile.1 <= self.max.1
    }
}

/// Where a `JobProgress::Haul` worker acquires the material before hauling it
/// to the blueprint. `Storage` (the default) is the legacy withdraw-from-
/// faction-storage path. `Market` routes the worker to the faction's market
/// node to buy the material with treasury-funded escrow capital, then reuses
/// the same `HaulToBlueprint` deposit leg. `max_unit_price` is the price
/// ceiling locked at posting time so a later price spike can't overspend the
/// escrowed purchase pool.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum HaulSource {
    #[default]
    Storage,
    Market {
        max_unit_price: f32,
    },
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
        /// Acquisition source for the hauled material. Defaults to `Storage`
        /// (legacy behaviour). `Market` is stamped by Phase 3c when the
        /// material is scarce-but-procurable for a state-authored blueprint.
        source: HaulSource,
    },
    /// Farm: seasonal field labor — prepare unworked soil, plant prepared
    /// soil, or harvest mature plants — scoped to a plot rect. `phase`
    /// drives the executor branch (Prepare / Plant / Harvest); `completed`
    /// counts tiles flipped (Prepare/Plant) or plants reaped (Harvest);
    /// `plot_id` / `assigned_farmer` populated when chief posts a
    /// plot-scoped Farm job (see seasonal-farming jellyfish plan).
    FieldWork {
        phase: crate::simulation::farm::FarmWorkPhase,
        completed: u32,
        target: u32,
        area: TileAabb,
        plot_id: Option<crate::simulation::land::PlotId>,
        assigned_farmer: Option<Entity>,
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
    /// Draftwork v2: tile-by-tile plowing of an Agricultural plot. The
    /// executor (`draftwork::plow_task_system`) accumulates per-tile work,
    /// bumps `plowed_tiles`, and on completion (`plowed_tiles >= target_tiles`)
    /// stamps `Plot.plowed_year = Some(calendar.year)`, releases the
    /// `AnimalWorkClaim`, and emits `JobCompletedEvent { completed: true }`.
    /// `assigned_worker` restricts claiming to one farmer; `animal` is
    /// populated by the dispatcher on first dispatch.
    Plow {
        plot_id: crate::simulation::land::PlotId,
        area: TileAabb,
        plowed_tiles: u32,
        target_tiles: u32,
        assigned_worker: Option<Entity>,
        animal: Option<Entity>,
    },
}

impl JobProgress {
    /// The specific resource this posting targets, if any. Drives Phase 3
    /// wage-signal keying so `Stockpile{wheat}` and `Stockpile{wood}`
    /// produce separate EMAs. `None` for postings without a single named
    /// target (Calories / Building / Planting).
    pub fn target_rid(&self) -> Option<crate::economy::resource_catalog::ResourceId> {
        use crate::economy::core_ids;
        match self {
            JobProgress::Calories { .. } => None,
            JobProgress::Stockpile { resource_id, .. } => Some(*resource_id),
            JobProgress::Haul { resource_id, .. } => Some(*resource_id),
            JobProgress::FieldWork { .. } => Some(core_ids::grain_seed()),
            JobProgress::Crafting { recipe, .. } => crate::simulation::crafting::craft_recipes()
                .get(*recipe as usize)
                .map(|r| r.output_resource),
            JobProgress::Building { .. } => None,
            // Plow's keying target is the ard plow implement (the gating
            // consumable in faction storage). Drives wage-signal EMA so
            // `Plow{ard_plow}` is its own bucket distinct from Farm.
            JobProgress::Plow { .. } => Some(core_ids::ard_plow()),
        }
    }

    pub fn is_complete(&self) -> bool {
        match self {
            JobProgress::Calories { deposited, target } => deposited >= target,
            JobProgress::Stockpile {
                deposited, target, ..
            } => deposited >= target,
            JobProgress::Haul {
                delivered, target, ..
            } => delivered >= target,
            JobProgress::FieldWork {
                completed, target, ..
            } => completed >= target,
            JobProgress::Crafting {
                crafted, target, ..
            } => crafted >= target,
            // Build completion is signalled externally by the despawn hook
            // (which removes the posting); this returns false because the
            // posting is removed before this would ever be re-checked.
            JobProgress::Building { .. } => false,
            JobProgress::Plow {
                plowed_tiles,
                target_tiles,
                ..
            } => plowed_tiles >= target_tiles,
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
            JobProgress::FieldWork {
                completed, target, ..
            } => {
                if *target == 0 {
                    1.0
                } else {
                    (*completed as f32 / *target as f32).clamp(0.0, 1.0)
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
            JobProgress::Plow {
                plowed_tiles,
                target_tiles,
                ..
            } => {
                if *target_tiles == 0 {
                    1.0
                } else {
                    (*plowed_tiles as f32 / *target_tiles as f32).clamp(0.0, 1.0)
                }
            }
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
    /// Settlement-scoped construction authority (sleepy-dove). Authors
    /// build/haul postings whose blueprint the chief couldn't gate
    /// because the chief hasn't personally Learned the construction tech.
    Architect,
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
            progress: JobProgress::Calories {
                deposited: 0,
                target: 1,
            },
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

/// What kind of resource a `ClaimTarget` accepts. Mirrors the design of
/// `MemoryKind` in `memory.rs`. `Specific(rid)` covers Stockpile/Haul of one
/// good; `AnyEdible` covers chief Calorie postings (intrinsically multi-
/// resource — Fruit/Meat/Grain/etc. all satisfy it); `None` covers Build/Plant
/// claims that don't bind to a single resource identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ClaimKind {
    #[default]
    None,
    Specific(crate::economy::resource_catalog::ResourceId),
    AnyEdible,
}

/// Companion component to `JobClaim` carrying the concrete target of the
/// currently held posting. Populated/refreshed by `job_goal_lock_system` so
/// plan resolvers can route to the claimed blueprint or resource without
/// re-querying the `JobBoard`. `kind = None` means the claim's posting kind
/// doesn't bind a resource (e.g. Build); `AnyEdible` means any catalog
/// resource with `edible_calories` satisfies the claim.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ClaimTarget {
    pub blueprint: Option<Entity>,
    pub kind: ClaimKind,
    /// Step 5: for `JobKind::Haul` claims, the posting's `HaulSource`
    /// snapshot so the dispatcher can pick the Market-buy chain without
    /// re-querying the `JobBoard`. `None` for non-Haul / Storage hauls
    /// (the default — keeps every other claim path byte-identical).
    pub haul_source: Option<HaulSource>,
}

impl ClaimTarget {
    /// Back-compat accessor: returns `Some(rid)` only for `Specific`. Callers
    /// that want "the one resource this claim binds" should use this instead
    /// of pattern-matching `kind` directly.
    pub fn resource_id(&self) -> Option<crate::economy::resource_catalog::ResourceId> {
        match self.kind {
            ClaimKind::Specific(r) => Some(r),
            _ => None,
        }
    }

    /// True if a deposit of `rid` would credit this claim.
    pub fn accepts(&self, rid: crate::economy::resource_catalog::ResourceId) -> bool {
        match self.kind {
            ClaimKind::Specific(r) => r == rid,
            ClaimKind::AnyEdible => rid.is_edible(),
            ClaimKind::None => false,
        }
    }

    /// True if this claim binds to food — either `AnyEdible` (chief Calories
    /// postings) or a `Specific(rid)` where `rid.is_edible()` (Stockpile of a
    /// specific food good). Drives `htn_stockpile_food_dispatch_system`'s gate.
    pub fn is_food(&self) -> bool {
        match self.kind {
            ClaimKind::AnyEdible => true,
            ClaimKind::Specific(r) => r.is_edible(),
            ClaimKind::None => false,
        }
    }
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
    /// Wage pool — paid to claimants on completion via `job_payout_system`,
    /// refunded to `beneficiary` on cancellation/expiry via the remove hook.
    pub amount: f32,
    pub beneficiary: Entity,
    /// Procurement capital for `HaulSource::Market` hauls (Step 4). Sized at
    /// funding time as `max_unit_price * target`. The worker draws an advance
    /// from this pool to buy the material and returns the unspent remainder
    /// in the same atomic step (Step 5); whatever residual survives to
    /// completion is refunded to `beneficiary` (Step 6). `0.0` for every
    /// non-Market posting — keeps the wage-only paths byte-identical.
    pub purchase_pool: f32,
}

impl JobEscrow {
    /// Total currency held by this escrow (wage + unspent procurement
    /// capital). Used by the remove hook and the system-wide invariant.
    pub fn held(&self) -> f32 {
        self.amount + self.purchase_pool
    }
}

pub fn on_job_escrow_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(escrow) = world.get::<JobEscrow>(entity).copied() else {
        return;
    };
    let refund = escrow.held();
    if !(refund > 0.0) {
        // Cleared on successful payout; nothing to refund.
        return;
    }
    if let Some(mut econ) =
        world.get_mut::<crate::economy::agent::EconomicAgent>(escrow.beneficiary)
    {
        econ.currency += refund;
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
    q.iter(world).map(|e| e.held()).sum()
}

/// Phase 0 (wage payout): map from `JobId → escrow sidecar entity` so
/// `job_payout_system` can find the escrow at completion time without
/// walking every entity in the world. Populated at posting-creation
/// sites (post_craft_contract / post_craft_contract_from_treasury /
/// post_stockpile_self / `chief_post_funding_system` for chief postings
/// whose faction funds public works or runs market-haul) and drained at
/// completion / cancellation by `job_payout_system`. Subsistence chief
/// postings (`chief_wage_for == 0` and no `purchase_pool`) carry no
/// escrow — their `JobId` is simply absent from this index.
#[derive(Resource, Default)]
pub struct JobEscrowIndex(pub AHashMap<JobId, Entity>);

/// Per-agent earnings ring. Phase 0 stub — populated by
/// `job_payout_system` on every paid completion. Phase 3 reads these
/// to drive the per-faction wage-signal EMA.
#[derive(Component, Clone, Debug, Default)]
pub struct Earnings {
    pub recent: std::collections::VecDeque<EarningEntry>,
}

#[derive(Clone, Copy, Debug)]
pub struct EarningEntry {
    pub job_kind: JobKind,
    /// Specific resource the posting targeted (e.g. `wheat` for
    /// `Stockpile{wheat}`). Phase 3 wage-signal keys on `(job_kind,
    /// target_rid)`. `None` when the posting wasn't resource-specific
    /// (food calories, build, plant).
    pub target_rid: Option<crate::economy::resource_catalog::ResourceId>,
    pub amount: f32,
    pub tick: u32,
}

impl Earnings {
    pub const CAP: usize = 16;
    pub fn push(&mut self, e: EarningEntry) {
        if self.recent.len() >= Self::CAP {
            self.recent.pop_front();
        }
        self.recent.push_back(e);
    }
}

/// Phase 0 (wage payout): drain `JobCompletedEvent`s, find the matching
/// escrow via `JobEscrowIndex`, and either pay claimants (completed
/// successfully) or despawn-with-refund (cancelled / expired). The
/// `JobEscrow.on_remove` hook handles the refund branch automatically —
/// the system just despawns the sidecar with `amount > 0`.
///
/// Wage split: `amount / claimants.len()` per worker, paid via `pay()`.
/// If `claimants` is empty on a `completed=true` event, the escrow is
/// refunded to its beneficiary (no worker to pay). Beneficiary-paying-
/// themselves is a no-op via `pay()`'s same-account guard? — `pay()`
/// permits self-transfer, so we explicitly skip when `worker == benef`.
pub fn job_payout_system(world: &mut World) {
    use bevy::ecs::event::Events;
    let events: Vec<JobCompletedEvent> = {
        let mut ev_res = world.resource_mut::<Events<JobCompletedEvent>>();
        ev_res.drain().collect()
    };
    if events.is_empty() {
        return;
    }
    let now = world.resource::<SimClock>().tick as u32;
    for ev in events {
        let escrow_entity = {
            let mut idx = world.resource_mut::<JobEscrowIndex>();
            idx.0.remove(&ev.job_id)
        };
        let Some(escrow_entity) = escrow_entity else {
            continue;
        };
        let Some(escrow) = world.get::<JobEscrow>(escrow_entity).copied() else {
            continue;
        };
        let beneficiary = escrow.beneficiary;
        let amount = escrow.amount;

        if ev.completed && !ev.claimants.is_empty() && amount > 0.0 {
            // Filter out the beneficiary themselves so a self-poster
            // who also worked the job doesn't shuffle currency back
            // into their own wallet.
            let payable: Vec<Entity> = ev
                .claimants
                .iter()
                .copied()
                .filter(|&w| w != beneficiary)
                .collect();
            let n = payable.len().max(1);
            let share = amount / n as f32;
            let mut paid_total = 0.0_f32;
            for worker in payable {
                // Phase 5b: apprentice claimants take a reduced share;
                // their mentor collects a small fee; the residual stays
                // in the escrow and refunds to the beneficiary on
                // despawn. Currency invariant preserved end-to-end —
                // apprentice + mentor + residual = `share`.
                let apprentice_mentor: Option<Entity> = world
                    .get::<crate::simulation::apprenticeship::ApprenticeOf>(worker)
                    .map(|link| link.mentor);
                let (worker_pay, mentor_pay) = if apprentice_mentor.is_some() {
                    (
                        share * crate::simulation::apprenticeship::WAGE_FRACTION_APPRENTICE,
                        share * crate::simulation::apprenticeship::WAGE_FRACTION_MENTOR_FEE,
                    )
                } else {
                    (share, 0.0)
                };

                // Direct escrow → worker credit. The escrow already
                // holds the funds (debited at posting time); we don't
                // re-debit the beneficiary. Invariant: agents_total
                // gains `worker_pay + mentor_pay`, escrowed loses
                // the same amount, net zero.
                let credited = {
                    if let Some(mut to_agent) =
                        world.get_mut::<crate::economy::agent::EconomicAgent>(worker)
                    {
                        to_agent.currency += worker_pay;
                        true
                    } else {
                        false
                    }
                };
                if credited {
                    paid_total += worker_pay;
                    // Log the earning on the worker.
                    if let Some(mut earnings) = world.get_mut::<Earnings>(worker) {
                        earnings.push(EarningEntry {
                            job_kind: ev.kind,
                            target_rid: ev.target_rid,
                            amount: worker_pay,
                            tick: now,
                        });
                    } else {
                        // Insert a fresh ring on first payout so we
                        // don't require every Person spawn site to
                        // bundle `Earnings`.
                        let mut e = Earnings::default();
                        e.push(EarningEntry {
                            job_kind: ev.kind,
                            target_rid: ev.target_rid,
                            amount: worker_pay,
                            tick: now,
                        });
                        world.entity_mut(worker).insert(e);
                    }
                    // Activity-log surfacing.
                    let faction_id = ev.faction_id;
                    let kind = ev.kind;
                    let mut events_log = world
                        .resource_mut::<bevy::ecs::event::Events<
                            crate::ui::activity_log::ActivityLogEvent,
                        >>();
                    events_log.send(crate::ui::activity_log::ActivityLogEvent {
                        tick: now as u64,
                        actor: worker,
                        faction_id,
                        kind: crate::ui::activity_log::ActivityEntryKind::WagePaid {
                            amount: worker_pay,
                            kind,
                        },
                    });
                }

                // Mentor fee — only credited if mentor is alive and
                // is not the beneficiary (avoid self-shuffling for
                // the rare case of a mentor-funded posting). Failed
                // mentor lookup leaves the residual in the escrow,
                // which refunds to the beneficiary on despawn.
                if let Some(mentor_entity) = apprentice_mentor {
                    if mentor_pay > 0.0 && mentor_entity != beneficiary {
                        let mentor_credited = {
                            if let Some(mut to_agent) =
                                world.get_mut::<crate::economy::agent::EconomicAgent>(mentor_entity)
                            {
                                to_agent.currency += mentor_pay;
                                true
                            } else {
                                false
                            }
                        };
                        if mentor_credited {
                            paid_total += mentor_pay;
                            if let Some(mut earnings) = world.get_mut::<Earnings>(mentor_entity) {
                                earnings.push(EarningEntry {
                                    job_kind: ev.kind,
                                    target_rid: ev.target_rid,
                                    amount: mentor_pay,
                                    tick: now,
                                });
                            } else {
                                let mut e = Earnings::default();
                                e.push(EarningEntry {
                                    job_kind: ev.kind,
                                    target_rid: ev.target_rid,
                                    amount: mentor_pay,
                                    tick: now,
                                });
                                world.entity_mut(mentor_entity).insert(e);
                            }
                            let faction_id = ev.faction_id;
                            let kind = ev.kind;
                            let mut events_log =
                                world.resource_mut::<bevy::ecs::event::Events<
                                    crate::ui::activity_log::ActivityLogEvent,
                                >>();
                            events_log.send(crate::ui::activity_log::ActivityLogEvent {
                                tick: now as u64,
                                actor: mentor_entity,
                                faction_id,
                                kind: crate::ui::activity_log::ActivityEntryKind::WagePaid {
                                    amount: mentor_pay,
                                    kind,
                                },
                            });
                        }
                    }
                }
            }
            // Zero the escrow (so on_remove hook is a no-op for the
            // paid portion) and despawn. Any residual rounds back to
            // the beneficiary via the hook.
            if let Some(mut e) = world.get_mut::<JobEscrow>(escrow_entity) {
                e.amount = (amount - paid_total).max(0.0);
            }
            world.entity_mut(escrow_entity).despawn();
        } else {
            // Cancellation / expiry / no claimants on completion:
            // despawn the escrow; the `on_remove` hook refunds.
            world.entity_mut(escrow_entity).despawn();
        }
    }
}

/// Defence-in-depth GC for `JobEscrowIndex`. The primary leak is closed
/// by emitting `JobCompletedEvent { completed: false }` from every posting
/// drop path; this sweep catches any future poster path that forgets, and
/// reaps index entries whose escrow entity has already despawned.
///
/// Runs daily in Economy after `job_payout_system`. Walks index entries:
/// - escrow entity gone but index entry remains → drop the entry.
/// - escrow entity alive but no posting on any faction board references
///   the `JobId` → despawn the escrow (the `on_remove` hook refunds) and
///   drop the entry.
pub fn escrow_index_gc_system(world: &mut World) {
    use crate::world::seasons::TICKS_PER_DAY;
    let tick = world.resource::<SimClock>().tick;
    if tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    let entries: Vec<(JobId, Entity)> = {
        let idx = world.resource::<JobEscrowIndex>();
        idx.0.iter().map(|(k, v)| (*k, *v)).collect()
    };
    if entries.is_empty() {
        return;
    }
    let mut stale_ids: Vec<JobId> = Vec::new();
    let mut orphan_entities: Vec<(JobId, Entity)> = Vec::new();
    for (job_id, escrow_entity) in entries {
        let escrow_alive = world.get::<JobEscrow>(escrow_entity).is_some();
        if !escrow_alive {
            stale_ids.push(job_id);
            continue;
        }
        let posting_exists = world.resource::<JobBoard>().locate(job_id).is_some();
        if !posting_exists {
            orphan_entities.push((job_id, escrow_entity));
        }
    }
    if stale_ids.is_empty() && orphan_entities.is_empty() {
        return;
    }
    {
        let mut idx = world.resource_mut::<JobEscrowIndex>();
        for job_id in &stale_ids {
            idx.0.remove(job_id);
        }
        for (job_id, _) in &orphan_entities {
            idx.0.remove(job_id);
        }
    }
    for (_, escrow_entity) in orphan_entities {
        // `on_remove` hook refunds wage + purchase_pool to the beneficiary.
        world.entity_mut(escrow_entity).despawn();
    }
}

// ─── Phase 4a — Chief-funded postings (Mixed / Market only) ─────────────

/// Chief pays half of catalog trade value for delivered material, by
/// default; tunable per kind below. Half captures "chief beats catalog
/// price by enough to draw labor, but is no worse than free agents
/// could earn on the market".
pub const CHIEF_MARGIN: f32 = 0.5;
/// Daily wage paid to claimants of a chief `Build` posting. Bounded
/// per-posting by [`CHIEF_BUILD_WAGE_CAP`] so a huge wall doesn't drain
/// the treasury.
pub const CHIEF_BUILD_WAGE_PER_DAY: f32 = 3.0;
pub const CHIEF_BUILD_WAGE_CAP: f32 = 30.0;
pub const CHIEF_BUILD_EXPECTED_DAYS: f32 = 5.0;
/// Daily wage paid to claimants of a chief `Farm` (planting) posting.
pub const CHIEF_FARM_WAGE_PER_DAY: f32 = 2.0;
pub const CHIEF_FARM_EXPECTED_DAYS: f32 = 4.0;
/// Daily wage for `Stockpile` Calories postings (food gather + deposit).
/// Calories postings carry no `target_rid` so trade_base_value can't be
/// keyed; use a flat per-day allowance instead.
pub const CHIEF_FOOD_WAGE_PER_DAY: f32 = 2.5;
pub const CHIEF_FOOD_EXPECTED_DAYS: f32 = 3.0;

/// Compute the chief's offered wage for a posting in faction
/// currency. Reads `JobProgress` to recover the (qty, target_rid)
/// information; returns `0.0` for shapes the chief can't sensibly
/// price (Building blueprints that already aren't scaled by qty —
/// handled via expected_days).
pub fn chief_wage_for(progress: &JobProgress) -> f32 {
    match progress {
        JobProgress::Stockpile {
            resource_id,
            target,
            ..
        } => {
            let base = resource_id.trade_base_value() as f32;
            base * (*target as f32) * CHIEF_MARGIN
        }
        JobProgress::Haul {
            resource_id,
            target,
            ..
        } => {
            // Haul is pure transport — pay less than Stockpile to keep
            // delivery cheaper than re-sourcing.
            let base = resource_id.trade_base_value() as f32;
            base * (*target as f32) * CHIEF_MARGIN * 0.5
        }
        JobProgress::Crafting { recipe, target, .. } => {
            let recipes = crate::simulation::crafting::craft_recipes();
            let out_value = recipes
                .get(*recipe as usize)
                .map(|r| r.output_resource.trade_base_value() as f32)
                .unwrap_or(0.0);
            out_value * (*target as f32) * CHIEF_MARGIN
        }
        JobProgress::Calories { .. } => {
            (CHIEF_FOOD_WAGE_PER_DAY * CHIEF_FOOD_EXPECTED_DAYS).min(40.0)
        }
        JobProgress::FieldWork { target, .. } => {
            let wage = CHIEF_FARM_WAGE_PER_DAY * CHIEF_FARM_EXPECTED_DAYS;
            // Scale up with target tile count, bounded so a 50-tile
            // farm doesn't crater treasury.
            (wage + (*target as f32) * 0.2).min(40.0)
        }
        JobProgress::Building { .. } => {
            (CHIEF_BUILD_WAGE_PER_DAY * CHIEF_BUILD_EXPECTED_DAYS).min(CHIEF_BUILD_WAGE_CAP)
        }
        JobProgress::Plow { target_tiles, .. } => {
            // Plow uses the same per-day base as Farm + scales with the
            // tile count. Cap matches Farm so a 16×16 = 256-tile plot
            // doesn't run treasury dry.
            let wage = CHIEF_FARM_WAGE_PER_DAY * CHIEF_FARM_EXPECTED_DAYS;
            (wage + (*target_tiles as f32) * 0.2).min(40.0)
        }
    }
}

/// Phase 4a: scan freshly-posted chief postings and attempt to fund
/// them out of the faction treasury. Postings in Mixed/Market factions
/// (those with a non-empty `economic_policy` map) become paid contracts
/// at `chief_wage_for(progress)`; postings in Subsistence factions stay
/// unpaid (matches the prior reward-0 behavior).
///
/// Insufficient treasury → posting stays at `reward = 0`. This couples
/// chief coordination to fiscal health: bankrupt factions can't direct
/// paid labor and fall back to communal work.
///
/// Runs in `SimulationSet::Economy`, exclusive `&mut World`, after
/// `chief_job_posting_system`. Funds only chief-source postings with
/// `reward == 0.0` (so it's idempotent — already-funded postings are
/// skipped; player/individual postings carry their own escrow).
pub fn chief_post_funding_system(world: &mut World) {
    // 1. Snapshot candidate postings: (job_id, faction_id, wage, chief_entity).
    //    Skip postings already funded, non-chief sources, or factions
    //    without a chief / without a non-empty policy map.
    // (job_id, faction_id, wage, purchase_pool, chief)
    let mut candidates: Vec<(JobId, u32, f32, f32, Entity)> = Vec::new();
    {
        let registry = world.resource::<crate::simulation::faction::FactionRegistry>();
        let board = world.resource::<JobBoard>();
        for (&faction_id, postings) in board.postings.iter() {
            let Some(faction) = registry.factions.get(&faction_id) else {
                continue;
            };
            // Subsistence: empty policy map → unpaid communal labor.
            if faction.economic_policy.is_empty() {
                continue;
            }
            let Some(chief) = faction.chief_entity else {
                continue;
            };
            for p in postings.iter() {
                if !matches!(p.source, JobSource::Chief) {
                    continue;
                }
                if p.reward > 0.0 {
                    continue;
                }
                let wage = chief_wage_for(&p.progress);
                if wage <= 0.0 {
                    continue;
                }
                // Market hauls also need procurement capital escrowed:
                // `max_unit_price * target`. Sized at funding time; the
                // worker draws an advance and returns the remainder atomically
                // (Step 5), residual refunds to the chief on completion
                // (Step 6). Zero for every other posting.
                let purchase_pool = match &p.progress {
                    JobProgress::Haul {
                        source: HaulSource::Market { max_unit_price },
                        target,
                        ..
                    } => max_unit_price.max(0.0) * (*target as f32),
                    _ => 0.0,
                };
                candidates.push((p.id, faction_id, wage, purchase_pool, chief));
            }
        }
    }
    if candidates.is_empty() {
        return;
    }

    // 2. For each candidate, check treasury, debit, spawn escrow, set
    //    reward, index it.
    for (job_id, faction_id, wage, purchase_pool, chief) in candidates {
        // Total escrowed = wage + procurement capital. The per-candidate
        // debit reads the live (already-debited) treasury, so concurrent
        // Market hauls in one pass are naturally serialized — no separate
        // running tally needed.
        let total = wage + purchase_pool;
        let funded = {
            let mut registry = world.resource_mut::<crate::simulation::faction::FactionRegistry>();
            let Some(faction) = registry.factions.get_mut(&faction_id) else {
                continue;
            };
            if faction.treasury < total {
                false
            } else {
                faction.treasury -= total;
                true
            }
        };
        if !funded {
            continue;
        }
        // Spawn escrow + index.
        let escrow_entity = world
            .spawn(JobEscrow {
                amount: wage,
                beneficiary: chief,
                purchase_pool,
            })
            .id();
        {
            let mut idx = world.resource_mut::<JobEscrowIndex>();
            idx.0.insert(job_id, escrow_entity);
        }
        // Set the posting's reward.
        {
            let mut board = world.resource_mut::<JobBoard>();
            if let Some(p) = board.get_mut(job_id) {
                p.reward = wage;
            }
        }
    }
}

/// Phase 3 (wage-aware-labor-market-v2): per-`(JobKind, target_rid)`
/// exponential moving average of payouts on this faction's postings.
/// Stored on `FactionData.wage_signal`. The aggregator
/// `faction_wage_signal_system` runs daily, sums each member's
/// last-day earnings into one nominal-currency total per key, then
/// folds via `ema_new = ALPHA * sample + (1 - ALPHA) * ema_old`.
///
/// `ALPHA = 1 − 0.5^(1/5) ≈ 0.129` matches a 5-day half-life: a wage
/// shock decays to half-amplitude after 5 days, supporting agents
/// reacting on a season-scale horizon. Zero-sample days decay the EMA
/// toward zero so an outage shows up in the signal.
#[derive(Clone, Copy, Debug, Default)]
pub struct WageEMA {
    /// Average wage earned per claimant on jobs of this key, in
    /// currency units per game-day. Phase 4 reads this as the
    /// expected payout when scoring `expected_wage(profession)`.
    pub ema_per_day: f32,
    pub last_update_tick: u32,
    /// Cumulative count of payouts folded into this EMA; informational
    /// for the inspector — not used in scoring.
    pub samples: u32,
}

const WAGE_EMA_ALPHA: f32 = 0.129_449_43;
const WAGE_EMA_WINDOW_TICKS: u32 = crate::world::seasons::TICKS_PER_DAY;

/// Phase 3 aggregator. Walks every agent's `Earnings` ring, sums per-
/// `(JobKind, target_rid)` payouts that landed in the last day, folds
/// them into the agent's *village* faction's `wage_signal`, then folds
/// zero into every other key on that faction's signal so the EMA decays
/// when no postings of that kind paid out. Runs once per day.
pub fn faction_wage_signal_system(world: &mut World) {
    let clock_tick = world.resource::<SimClock>().tick as u32;
    if clock_tick % WAGE_EMA_WINDOW_TICKS != 0 {
        return;
    }
    let window_start = clock_tick.saturating_sub(WAGE_EMA_WINDOW_TICKS);

    // 1. Snapshot per-(faction, key) sums from member earnings.
    let mut sums: ahash::AHashMap<
        (
            u32,
            JobKind,
            Option<crate::economy::resource_catalog::ResourceId>,
        ),
        (f32, u32),
    > = ahash::AHashMap::default();
    let mut q = world.query::<(&Earnings, &crate::simulation::faction::FactionMember)>();
    for (earnings, fm) in q.iter(world) {
        let village_id = {
            let registry = world.resource::<crate::simulation::faction::FactionRegistry>();
            registry.root_faction(fm.faction_id)
        };
        for entry in earnings.recent.iter() {
            if entry.tick < window_start {
                continue;
            }
            let key = (village_id, entry.job_kind, entry.target_rid);
            let slot = sums.entry(key).or_insert((0.0, 0));
            slot.0 += entry.amount;
            slot.1 = slot.1.saturating_add(1);
        }
    }

    // 2. Fold into wage_signal for every village faction with a non-
    //    empty key set (existing + freshly-paid).
    let mut registry = world.resource_mut::<crate::simulation::faction::FactionRegistry>();
    // Phase 1: process each faction's existing keys (decay path).
    let faction_ids: Vec<u32> = registry.factions.keys().copied().collect();
    for fid in faction_ids {
        let Some(faction) = registry.factions.get_mut(&fid) else {
            continue;
        };
        // Collect this faction's existing keys; needed because we may
        // also touch keys that don't appear in `sums` (zero-decay).
        let existing_keys: Vec<(
            JobKind,
            Option<crate::economy::resource_catalog::ResourceId>,
        )> = faction.wage_signal.keys().copied().collect();
        for key in existing_keys {
            let sample = sums
                .get(&(fid, key.0, key.1))
                .map(|(amt, n)| if *n > 0 { *amt / *n as f32 } else { 0.0 })
                .unwrap_or(0.0);
            let ema = faction.wage_signal.entry(key).or_default();
            ema.ema_per_day = WAGE_EMA_ALPHA * sample + (1.0 - WAGE_EMA_ALPHA) * ema.ema_per_day;
            ema.last_update_tick = clock_tick;
            if sample > 0.0 {
                ema.samples = ema.samples.saturating_add(1);
            }
        }
    }
    // Phase 2: fresh keys (paid for the first time on this faction).
    for ((fid, kind, rid), (amt, n)) in sums.into_iter() {
        let Some(faction) = registry.factions.get_mut(&fid) else {
            continue;
        };
        let sample = if n > 0 { amt / n as f32 } else { 0.0 };
        let key = (kind, rid);
        if faction.wage_signal.contains_key(&key) {
            continue; // already folded above
        }
        // Seed: jump straight to the sample so day-1 signals are
        // immediately usable rather than 13% of true.
        faction.wage_signal.insert(
            key,
            WageEMA {
                ema_per_day: sample,
                last_update_tick: clock_tick,
                samples: 1,
            },
        );
    }
}

/// Phase 3 cross-faction gossip surface: per-agent perception of
/// *other* factions' wage signals, refreshed by socialize encounters.
/// Same-faction work reads `FactionData.wage_signal` directly; this
/// component captures the information friction for cross-faction
/// migration / posting decisions that Phase 4+ will introduce.
///
/// Keys mirror `wage_signal` keys plus an outer `fid` so the agent
/// remembers per-faction signals separately. Value is `(ema_per_day,
/// observed_tick)` — the `observed_tick` lets stale gossip decay.
#[derive(Component, Clone, Debug, Default)]
pub struct PerceivedFactionWages {
    pub by_key: ahash::AHashMap<
        (
            u32,
            JobKind,
            Option<crate::economy::resource_catalog::ResourceId>,
        ),
        (f32, u32),
    >,
}

impl PerceivedFactionWages {
    pub const CAP: usize = 32;

    /// Merge `(fid, kind, rid, ema, observed_tick)` into this map,
    /// keeping the higher `observed_tick` on conflict. Evicts oldest
    /// entries past `CAP`.
    pub fn merge_entry(
        &mut self,
        fid: u32,
        kind: JobKind,
        rid: Option<crate::economy::resource_catalog::ResourceId>,
        ema: f32,
        observed_tick: u32,
    ) {
        let key = (fid, kind, rid);
        let take = match self.by_key.get(&key) {
            Some((_, t)) => observed_tick > *t,
            None => true,
        };
        if take {
            self.by_key.insert(key, (ema, observed_tick));
        }
        if self.by_key.len() > Self::CAP {
            // Evict oldest entry. Linear scan is fine at CAP=32.
            if let Some((evict, _)) = self
                .by_key
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, v)| (*k, *v))
            {
                self.by_key.remove(&evict);
            }
        }
    }
}

/// Phase 3 gossip: when two agents are socializing within 3 tiles,
/// each merges up to `WAGE_GOSSIP_TOP_K = 4` of the other's most-
/// recent wage observations (their own faction's `wage_signal` plus
/// any previously-gossiped `PerceivedFactionWages` entries) into their
/// own `PerceivedFactionWages`. Same-faction entries are skipped —
/// agents already see their own faction's signal directly.
///
/// Information friction is the point: wages spread across factions
/// only as fast as agents physically socialize.
pub const WAGE_GOSSIP_TOP_K: usize = 4;
pub const WAGE_GOSSIP_RADIUS: i32 = 3;

pub fn wage_gossip_system(
    spatial: Res<crate::world::spatial::SpatialIndex>,
    registry: Res<crate::simulation::faction::FactionRegistry>,
    clock: Res<SimClock>,
    mut q: Query<(
        Entity,
        &Transform,
        &crate::simulation::goals::AgentGoal,
        &crate::simulation::lod::LodLevel,
        &crate::simulation::faction::FactionMember,
        Option<&mut PerceivedFactionWages>,
        Option<&crate::simulation::social_contact::SecondarySocial>,
    )>,
    mut commands: Commands,
) {
    use crate::simulation::social_contact::is_social_contact;
    let now = clock.tick as u32;

    // Each socializing agent contributes up to TOP_K (fid, kind, rid,
    // ema, observed_tick) entries — drawn first from their own
    // faction's wage_signal (most credible), then their existing
    // perceived entries by recency.
    type GossipEntry = (
        u32,
        JobKind,
        Option<crate::economy::resource_catalog::ResourceId>,
        f32,
        u32,
    );
    let mut snapshots: ahash::AHashMap<Entity, Vec<GossipEntry>> = ahash::AHashMap::default();

    for (entity, _t, goal, lod, fm, perceived, sec) in q.iter() {
        if !is_social_contact(*goal, *lod, sec, now) {
            continue;
        }
        let village_id = registry.root_faction(fm.faction_id);
        let mut entries: Vec<GossipEntry> = Vec::new();
        if let Some(faction) = registry.factions.get(&village_id) {
            for (&(kind, rid), ema) in faction.wage_signal.iter() {
                if ema.ema_per_day <= 0.0 {
                    continue;
                }
                entries.push((village_id, kind, rid, ema.ema_per_day, ema.last_update_tick));
            }
        }
        if let Some(perc) = perceived.as_deref() {
            for (&(fid, kind, rid), &(ema, tick)) in perc.by_key.iter() {
                if ema <= 0.0 {
                    continue;
                }
                entries.push((fid, kind, rid, ema, tick));
            }
        }
        // Keep the top-K most recently observed entries.
        entries.sort_unstable_by_key(|(_, _, _, _, t)| std::cmp::Reverse(*t));
        entries.truncate(WAGE_GOSSIP_TOP_K);
        snapshots.insert(entity, entries);
    }

    if snapshots.is_empty() {
        return;
    }

    for (entity, transform, goal, lod, fm, perceived, sec) in q.iter_mut() {
        if !is_social_contact(*goal, *lod, sec, now) {
            continue;
        }
        let tx = (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
        let village_id = registry.root_faction(fm.faction_id);
        let mut to_merge: Vec<GossipEntry> = Vec::new();
        for dy in -WAGE_GOSSIP_RADIUS..=WAGE_GOSSIP_RADIUS {
            for dx in -WAGE_GOSSIP_RADIUS..=WAGE_GOSSIP_RADIUS {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    if let Some(snap) = snapshots.get(&other) {
                        for e in snap.iter() {
                            // Skip our own faction — we read it from
                            // wage_signal directly, no need to cache.
                            if e.0 == village_id {
                                continue;
                            }
                            to_merge.push(*e);
                        }
                    }
                }
            }
        }
        if to_merge.is_empty() {
            continue;
        }
        if let Some(mut perc) = perceived {
            for (fid, kind, rid, ema, tick) in to_merge {
                // Apply a small staleness penalty so older gossip
                // doesn't overwrite fresher first-hand observations.
                let age = now.saturating_sub(tick);
                let penalty = (-(age as f32) / WAGE_EMA_WINDOW_TICKS as f32).exp();
                let observed_ema = ema * penalty;
                perc.merge_entry(fid, kind, rid, observed_ema, tick);
            }
        } else {
            let mut perc = PerceivedFactionWages::default();
            for (fid, kind, rid, ema, tick) in to_merge {
                let age = now.saturating_sub(tick);
                let penalty = (-(age as f32) / WAGE_EMA_WINDOW_TICKS as f32).exp();
                perc.merge_entry(fid, kind, rid, ema * penalty, tick);
            }
            commands.entity(entity).insert(perc);
        }
    }
}

/// Pluralist Economy R12: post a P2P craft contract. The `poster`
/// (an individual agent with surplus currency, typically Esteem-
/// driven) commissions a craft job paying `reward` on completion.
/// On success: poster's currency is debited; a `JobPosting` is
/// added to the board with `poster_class=Individual`; a sidecar
/// entity carrying `JobEscrow { amount: reward, beneficiary: poster }`
/// is spawned. Returns the spawned escrow entity (so the caller can
/// pair the posting with its escrow for completion / cancellation).
///
/// On insufficient funds, missing `EconomicAgent`, or invalid recipe:
/// returns `None` and does not mutate state.
///
/// Lifecycle: when the smith completes the craft, the poster (or
/// the completion system) clears `escrow.amount = 0.0` then despawns
/// the escrow — the `on_job_escrow_remove` hook is a no-op. On
/// cancellation/expiry, despawning the escrow refunds `amount` to
/// `poster` automatically. All 25 existing `aq.cancel()` sites stay
/// untouched.
pub fn post_craft_contract(
    world: &mut World,
    poster: Entity,
    faction_id: u32,
    recipe: RecipeId,
    qty: u32,
    reward: f32,
    deadline_tick: Option<u32>,
) -> Option<Entity> {
    if !(reward > 0.0) || qty == 0 {
        return None;
    }
    if crate::simulation::crafting::craft_recipes()
        .get(recipe as usize)
        .is_none()
    {
        return None;
    }
    // Funds check + atomic debit.
    let poster_currency = world
        .get::<crate::economy::agent::EconomicAgent>(poster)?
        .currency;
    if poster_currency < reward {
        return None;
    }
    {
        let mut econ = world.get_mut::<crate::economy::agent::EconomicAgent>(poster)?;
        econ.currency -= reward;
    }

    // Allocate posting id + push.
    let posted_tick = world
        .get_resource::<SimClock>()
        .map(|c| c.tick as u32)
        .unwrap_or(0);
    let job_id = {
        let mut board = world.resource_mut::<JobBoard>();
        let id = board.alloc_id();
        let progress = JobProgress::Crafting {
            crafted: 0,
            target: qty,
            recipe,
            // Bench/tech_payload: P2P contracts don't pre-pick a
            // bench; the smith's claim path resolves a Workbench
            // within range. tech_payload is None — knowledge
            // recipes (tablet/book) don't go through this path.
            bench: None,
            tech_payload: None,
        };
        board.faction_postings_mut(faction_id).push(JobPosting {
            id,
            faction_id,
            kind: JobKind::Craft,
            progress,
            claimants: Vec::new(),
            priority: PLAYER_PRIORITY,
            source: JobSource::Player,
            posted_tick,
            expiry_tick: deadline_tick,
            poster_class: PosterClass::Individual,
            reward,
            settlement_id: None,
        });
        id
    };

    // Spawn the escrow sidecar. The hook handles refund on despawn.
    let escrow = world
        .spawn(JobEscrow {
            amount: reward,
            beneficiary: poster,
            purchase_pool: 0.0,
        })
        .id();
    // Phase 0: register the escrow against the job_id so
    // `job_payout_system` can pay claimants on completion.
    world
        .resource_mut::<JobEscrowIndex>()
        .0
        .insert(job_id, escrow);
    Some(escrow)
}

/// Pluralist Economy R6 follow-on: post a craft contract funded
/// from a faction's treasury (rather than an agent's wallet). Used
/// by `household_contract_posting_system` so households can
/// commission work without their head personally fronting the
/// currency.
///
/// Behaviour mirrors `post_craft_contract` except the debit/refund
/// flows through `FactionData.treasury` (looked up via
/// `FactionRegistry`). The escrow sidecar's `beneficiary` is set to
/// a "treasury beneficiary" placeholder Entity (today: the
/// household head, since on cancellation the refund would otherwise
/// vanish — the head holds the household's currency by proxy and
/// the household's `treasury` is recredited at completion via a
/// future system). For R6's narrow validation (post + escrow
/// lifecycle) we use the head as beneficiary; future R-phases can
/// add a Treasury-typed beneficiary if needed.
///
/// Returns the spawned escrow entity on success, or None on:
/// insufficient treasury, missing recipe, missing faction, qty=0
/// or non-positive reward.
pub fn post_craft_contract_from_treasury(
    world: &mut World,
    funding_faction_id: u32,
    posting_faction_id: u32,
    head: Entity,
    recipe: RecipeId,
    qty: u32,
    reward: f32,
    deadline_tick: Option<u32>,
) -> Option<Entity> {
    if !(reward > 0.0) || qty == 0 {
        return None;
    }
    if crate::simulation::crafting::craft_recipes()
        .get(recipe as usize)
        .is_none()
    {
        return None;
    }
    // Treasury check + atomic debit.
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        let funding = registry.factions.get_mut(&funding_faction_id)?;
        if funding.treasury < reward {
            return None;
        }
        funding.treasury -= reward;
    }

    let posted_tick = world
        .get_resource::<SimClock>()
        .map(|c| c.tick as u32)
        .unwrap_or(0);
    let job_id = {
        let mut board = world.resource_mut::<JobBoard>();
        let id = board.alloc_id();
        let progress = JobProgress::Crafting {
            crafted: 0,
            target: qty,
            recipe,
            bench: None,
            tech_payload: None,
        };
        board
            .faction_postings_mut(posting_faction_id)
            .push(JobPosting {
                id,
                faction_id: posting_faction_id,
                kind: JobKind::Craft,
                progress,
                claimants: Vec::new(),
                priority: PLAYER_PRIORITY,
                source: JobSource::Player,
                posted_tick,
                expiry_tick: deadline_tick,
                poster_class: PosterClass::HouseholdHead,
                reward,
                settlement_id: None,
            });
        id
    };

    // Escrow with the head as beneficiary. On cancellation, the
    // refund lands in the head's wallet — this is a small
    // deviation from "treasury-funded" semantics but keeps
    // currency-conservation strict without requiring a Treasury
    // entity primitive. R-future phases can swap the beneficiary
    // for a treasury proxy if desired.
    let escrow = world
        .spawn(JobEscrow {
            amount: reward,
            beneficiary: head,
            purchase_pool: 0.0,
        })
        .id();
    world
        .resource_mut::<JobEscrowIndex>()
        .0
        .insert(job_id, escrow);
    Some(escrow)
}

// ─── P4 (minimal): self-posted Stockpile contracts ─────────────────

/// Wage margin applied to every self-posted Stockpile contract. The
/// formula `wage = trade_base_value(rid) * target_qty * SELF_POST_MARGIN`
/// gives a small but non-trivial reward — large enough to outscore a
/// chief Stockpile (`reward = 0`) on the `U_bid` scorer, small enough
/// not to drain self-posters' treasuries on a single contract. Tunable.
pub const SELF_POST_MARGIN: f32 = 0.1;

/// Self-posted Stockpile wage formula. Derives from the catalog's
/// authored `trade_base_value` (sedentarize trace 3 finding: market
/// price is dead code for raw resources in early game; recipe cost
/// floors don't exist for gathered resources; `trade_base_value` is
/// the intentional single source of truth). Returns the absolute wage
/// in currency for `target_qty` units.
pub fn self_post_wage(
    catalog: &crate::economy::resource_catalog::ResourceCatalog,
    rid: crate::economy::resource_catalog::ResourceId,
    target_qty: u32,
) -> f32 {
    let base = catalog
        .iter()
        .find(|(id, _)| *id == rid)
        .map(|(_, def)| def.trade_base_value as f32)
        .unwrap_or(1.0);
    base * target_qty as f32 * SELF_POST_MARGIN
}

/// Post a Stockpile contract funded from `author`'s wallet (or, for
/// `PosterClass::HouseholdHead`, the household treasury — caller
/// chooses by passing the right beneficiary). Mirrors
/// `post_craft_contract`'s shape for the Stockpile case.
///
/// On insufficient funds / missing `EconomicAgent` / qty=0: returns
/// `None`, no state mutation.
///
/// **P4 minimal scope: this helper is additive — it's not yet wired
/// into `goal_update_system`'s autonomous fallback.** Closing the
/// posting bypass at the goal level (and the Subsistence regression
/// invariant) is a focused future session.
pub fn post_stockpile_self(
    world: &mut World,
    author: Entity,
    faction_id: u32,
    resource_id: crate::economy::resource_catalog::ResourceId,
    target_qty: u32,
    poster_class: PosterClass,
    deadline_tick: Option<u32>,
) -> Option<Entity> {
    if target_qty == 0 {
        return None;
    }
    let catalog = world
        .resource::<crate::economy::resource_catalog::ResourceCatalog>()
        .clone();
    let reward = self_post_wage(&catalog, resource_id, target_qty);
    if !(reward > 0.0) {
        return None;
    }

    // Funds check + atomic debit.
    let author_currency = world
        .get::<crate::economy::agent::EconomicAgent>(author)?
        .currency;
    if author_currency < reward {
        return None;
    }
    {
        let mut econ = world.get_mut::<crate::economy::agent::EconomicAgent>(author)?;
        econ.currency -= reward;
    }

    let posted_tick = world
        .get_resource::<SimClock>()
        .map(|c| c.tick as u32)
        .unwrap_or(0);
    let job_id = {
        let mut board = world.resource_mut::<JobBoard>();
        let id = board.alloc_id();
        let progress = JobProgress::Stockpile {
            resource_id,
            deposited: 0,
            target: target_qty,
        };
        board.faction_postings_mut(faction_id).push(JobPosting {
            id,
            faction_id,
            kind: JobKind::Stockpile,
            progress,
            claimants: Vec::new(),
            priority: PLAYER_PRIORITY,
            source: JobSource::Player,
            posted_tick,
            expiry_tick: deadline_tick,
            poster_class,
            reward,
            settlement_id: None,
        });
        id
    };

    let escrow = world
        .spawn(JobEscrow {
            amount: reward,
            beneficiary: author,
            purchase_pool: 0.0,
        })
        .id();
    world
        .resource_mut::<JobEscrowIndex>()
        .0
        .insert(job_id, escrow);
    Some(escrow)
}

// ─── P4 full: worker self-post for staple Stockpile contracts ─────────
//
// In Market-mode factions the chief stops posting Stockpile{wood/stone}
// (chief_allocates_labor=false), but workers still need raw materials
// flowing into faction storage. Without this system, Market workers
// fall through to the autonomous gather goal in goal_update_system —
// they gather "for free", earning nothing despite being in a market
// economy. With this system, once per game-day a wealthy worker self-
// posts a small Stockpile contract; another worker (often themselves)
// claims and earns the wage. Subsistence factions are untouched
// (their staples are chief-allocated, so the gate skips them).

/// Cadence for `worker_self_post_stockpile_system`. Once per game-day
/// matches the lifetime of a typical contract — long enough for
/// claim-and-deliver round-trips, short enough to feel responsive.
pub const WORKER_SELF_POST_CADENCE: u64 = crate::world::seasons::TICKS_PER_DAY as u64;
/// Default target quantity for a self-posted Stockpile contract. Small
/// enough that the wage stays affordable for early-game agents (wage =
/// `trade_base_value(rid) * qty * SELF_POST_MARGIN` ≈ 5–8 currency at
/// qty=10), large enough to make the posting worth claiming.
pub const WORKER_SELF_POST_TARGET_QTY: u32 = 10;
/// Floor on the author's currency. Below this we don't drain a near-
/// destitute worker to fund a contract — they need the runway for
/// market food purchases more than they need raw materials posted.
pub const WORKER_SELF_POST_MIN_CURRENCY: f32 = 20.0;

/// Worker-funded Stockpile poster. Runs daily, exclusive to allow
/// `post_stockpile_self`'s atomic debit + JobBoard push + JobEscrow
/// spawn. For each non-household, non-nomadic faction whose staple
/// policy disables chief allocation, picks the wealthiest claim-free
/// member and self-posts a Stockpile contract for any staple resource
/// the faction has a deficit on AND knows a cluster for AND has no
/// live Stockpile posting on already.
pub fn worker_self_post_stockpile_system(world: &mut World) {
    let tick = world.resource::<SimClock>().tick;
    if tick % WORKER_SELF_POST_CADENCE != 0 {
        return;
    }

    let wood_id = crate::economy::core_ids::wood();
    let stone_id = crate::economy::core_ids::stone();
    let catalog = world
        .resource::<crate::economy::resource_catalog::ResourceCatalog>()
        .clone();

    // Phase 1: per-faction decisions (which staple resources need a
    // self-post). Snapshot the read-only state so we can mutate the
    // world freely in Phase 3 without conflicting borrows.
    #[derive(Clone, Copy)]
    struct Decision {
        faction_id: u32,
        rid: crate::economy::resource_catalog::ResourceId,
        qty: u32,
    }
    let decisions: Vec<Decision> = {
        let registry = world.resource::<FactionRegistry>();
        let board = world.resource::<JobBoard>();
        let shared = world.resource::<crate::simulation::shared_knowledge::SharedKnowledge>();
        let settlement_map = world.resource::<crate::simulation::settlement::SettlementMap>();

        let mut out = Vec::new();
        for (&fid, faction) in registry.factions.iter() {
            if faction.member_count == 0 {
                continue;
            }
            // Households post their own contracts via
            // household_contract_posting_system. Skip them here so
            // we don't double-post on the village's behalf.
            if faction.parent_faction.is_some() {
                continue;
            }
            // Nomadic / posting-disabled archetypes: capability gate
            // already mirrors the legacy is_nomadic() short-circuit.
            if faction.caps.posting.is_disabled() {
                continue;
            }
            // Phase C: a Packed (mobile) band is settled-life-paused.
            if matches!(
                faction.camp_state,
                crate::simulation::faction::CampState::Packed { .. }
            ) {
                continue;
            }

            for &target_rid in &[wood_id, stone_id] {
                // Chief still handles this resource? leave it alone.
                if faction.policy_for(target_rid).chief_allocates_labor {
                    continue;
                }
                // Already a Stockpile posting for this rid? skip —
                // duplicate posting just dilutes claim attention.
                let already = board.faction_postings(fid).iter().any(|p| {
                    matches!(
                        p.progress,
                        JobProgress::Stockpile { resource_id, .. } if resource_id == target_rid
                    )
                });
                if already {
                    continue;
                }

                // Real deficit on this staple? Mirrors the chief
                // branch's deficit gate so a fully-stocked faction
                // doesn't post for the sake of posting.
                let target = faction
                    .material_target_of(target_rid)
                    .max(faction.demand_of(target_rid));
                let stored = faction.storage.stock_of(target_rid);
                if target <= stored {
                    continue;
                }

                // Faction-tier cluster knowledge — same gate as the
                // chief branch. No known cluster ⇒ futile posting.
                if !crate::simulation::shared_knowledge::faction_knows_cluster(
                    &shared,
                    &settlement_map,
                    fid,
                    crate::simulation::memory::MemoryKind::Resource(target_rid),
                    (faction.home_tile.0 as i32, faction.home_tile.1 as i32),
                    16,
                ) {
                    continue;
                }

                out.push(Decision {
                    faction_id: fid,
                    rid: target_rid,
                    qty: WORKER_SELF_POST_TARGET_QTY,
                });
            }
        }
        out
    };

    if decisions.is_empty() {
        return;
    }

    // Phase 2: build per-faction wealth-ranked candidate lists. Skip
    // claim-holders (they're working) and Drafted (combat / lecture).
    let mut members_by_faction: ahash::AHashMap<u32, Vec<(Entity, f32)>> = ahash::AHashMap::new();
    {
        let mut q = world.query_filtered::<(
            Entity,
            &crate::simulation::faction::FactionMember,
            &crate::economy::agent::EconomicAgent,
        ), (
            Without<JobClaim>,
            Without<crate::simulation::person::Drafted>,
        )>();
        for (entity, member, econ) in q.iter(world) {
            members_by_faction
                .entry(member.faction_id)
                .or_default()
                .push((entity, econ.currency));
        }
    }
    for list in members_by_faction.values_mut() {
        list.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    }

    // Phase 3: enact decisions. One contract per (faction, rid) per
    // cadence — the wealthiest member who can afford the wage authors.
    for d in decisions {
        let Some(members) = members_by_faction.get(&d.faction_id) else {
            continue;
        };
        let wage = self_post_wage(&catalog, d.rid, d.qty);
        let Some(&(author, _currency)) = members
            .iter()
            .find(|&&(_, c)| c >= wage && c >= WORKER_SELF_POST_MIN_CURRENCY)
        else {
            continue;
        };
        post_stockpile_self(
            world,
            author,
            d.faction_id,
            d.rid,
            d.qty,
            PosterClass::Individual,
            None,
        );
    }
}

// ─── Pluralist Economy R8 follow-on: Esteem-driven posting ─────────

/// Per-individual reward when an Esteem-seeking agent commissions a
/// luxury good (Torch, recipe id 2). Anchored at 8.0 — slightly
/// above household-poster baseline so a satiated wealthy agent's
/// contract outscores a typical household contract on the U_bid
/// scorer.
pub const ESTEEM_CONTRACT_REWARD: f32 = 8.0;

/// Minimum agent currency required for the Esteem-driven posting
/// system to commission a contract. Above this, surplus currency
/// is "esteem-spendable"; below, the agent prioritises wealth
/// accumulation.
pub const ESTEEM_POSTING_MIN_CURRENCY: f32 = 50.0;

/// Cadence at which `esteem_driven_posting_system` runs. Once per
/// game-day; each qualifying agent posts at most one contract per
/// firing.
pub const ESTEEM_POSTING_CADENCE: u64 = crate::world::seasons::TICKS_PER_DAY as u64;

/// Per-tick `Needs.esteem` increment when an agent posts a
/// prestigious contract. The act of commissioning is what grants
/// status, not the eventual completion. Small enough that an agent
/// needs to keep posting (or earn esteem some other way) to stay
/// satiated.
pub const ESTEEM_POSTING_GAIN: f32 = 30.0;

/// Pluralist Economy R8 follow-on: Esteem-driven contract posting.
///
/// Walks every agent whose Maslow tier (`MaslowTier::next_unmet`)
/// is `Esteem` AND whose `EconomicAgent.currency` is above
/// `ESTEEM_POSTING_MIN_CURRENCY`. For each, posts a Luxury (Torch,
/// recipe id 2) contract via `post_craft_contract` with the agent
/// as poster — the act of commissioning prestige goods grants
/// `ESTEEM_POSTING_GAIN` to `Needs.esteem`.
///
/// **Critical**: this system is *additive*. It does not preempt
/// `goal_update_system`'s goal selection — the agent's `AgentGoal`
/// is unchanged. The contract is a behavioural side-effect of the
/// agent's wealth + Maslow profile. The posted contract enters the
/// regular U_bid claim flow (R9), so smiths see and claim it
/// alongside other paid postings.
///
/// Cadence: `ESTEEM_POSTING_CADENCE` (one game-day).
pub fn esteem_driven_posting_system(world: &mut World) {
    use crate::simulation::goals::MaslowTier;

    let now = match world.get_resource::<SimClock>() {
        Some(c) => c.tick,
        None => return,
    };
    if now % ESTEEM_POSTING_CADENCE != 0 {
        return;
    }

    let luxury = crate::economy::core_ids::luxury();

    // Functional gate (see craft-demand plan): dedup against any live
    // Luxury-output craft contract already on a faction's board.
    let mut faction_has_luxury_contract: ahash::AHashSet<u32> = ahash::AHashSet::default();
    {
        let board = world.resource::<JobBoard>();
        let recipes = crate::simulation::crafting::craft_recipes();
        for (&fid, postings) in board.postings.iter() {
            for p in postings.iter() {
                if let JobProgress::Crafting { recipe, .. } = &p.progress {
                    if recipes
                        .get(*recipe as usize)
                        .map_or(false, |r| r.output_resource == luxury)
                    {
                        faction_has_luxury_contract.insert(fid);
                        break;
                    }
                }
            }
        }
    }

    // Snapshot eligible agents (entity, faction_id) so we can later
    // mutate without holding query borrows during the posting calls
    // (which take &mut World).
    let mut intents: Vec<(Entity, u32)> = Vec::new();
    {
        let mut q = world.query::<(
            Entity,
            &crate::simulation::needs::Needs,
            &crate::economy::agent::EconomicAgent,
            &crate::simulation::items::Equipment,
            &crate::simulation::faction::FactionMember,
            &crate::simulation::lod::LodLevel,
        )>();
        for (entity, needs, econ, equipment, member, lod) in q.iter(world) {
            if *lod == crate::simulation::lod::LodLevel::Dormant {
                continue;
            }
            if member.faction_id == crate::simulation::faction::SOLO {
                continue;
            }
            if econ.currency < ESTEEM_POSTING_MIN_CURRENCY {
                continue;
            }
            if MaslowTier::next_unmet(needs) != Some(MaslowTier::Esteem) {
                continue;
            }
            // Skip when a Luxury contract is already live on the faction
            // board, or the poster already owns a Luxury good — esteem
            // posting requires a real lack of the output.
            if faction_has_luxury_contract.contains(&member.faction_id) {
                continue;
            }
            if econ.quantity_of_resource(luxury) > 0 || equipment.has_resource(luxury) {
                continue;
            }
            intents.push((entity, member.faction_id));
        }
    }

    for (poster, faction_id) in intents {
        let escrow = post_craft_contract(
            world,
            poster,
            faction_id,
            2, // Torch (Luxury). Paleolithic-tech, always available.
            1,
            ESTEEM_CONTRACT_REWARD,
            None,
        );
        if escrow.is_some() {
            // Reward the agent's psyche for the prestigious post.
            if let Some(mut needs) = world.get_mut::<crate::simulation::needs::Needs>(poster) {
                needs.esteem = (needs.esteem + ESTEEM_POSTING_GAIN).min(255.0);
            }
        }
    }
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
///
/// Phase 0 (wage payout) extension: events that represent *genuine work
/// completion* carry `completed = true` and the `claimants` who did the
/// work. `job_payout_system` reads these to split the matching escrow's
/// `amount` across the workers via `pay()` and despawn the escrow.
/// Cancellation / expiry / target-invalid paths set `completed = false`
/// and (usually) leave `claimants` empty; the payout system then despawns
/// the escrow with `amount > 0` so the `on_remove` hook refunds the
/// poster.
#[derive(Event, Clone, Debug)]
pub struct JobCompletedEvent {
    pub job_id: JobId,
    pub faction_id: u32,
    pub kind: JobKind,
    /// Workers who held a claim at the moment the posting was removed.
    /// Drained from the posting in the cleanup paths so the payout
    /// system doesn't need to walk a stale `JobBoard`.
    pub claimants: Vec<Entity>,
    /// `true` if the posting was removed because its target was met
    /// (record_progress completion, Haul-slot satisfied, Build blueprint
    /// despawn). `false` for cancellation / expiry / bench-invalidation
    /// paths — the payout system then just despawns the escrow so the
    /// `on_remove` hook refunds the poster.
    pub completed: bool,
    /// Phase 3: specific resource the posting targeted (e.g. `wheat` for
    /// `Stockpile{wheat}`); folded into `EarningEntry.target_rid` so the
    /// wage-signal aggregator can key `(kind, rid)` separately.
    /// `None` when the posting wasn't resource-specific.
    pub target_rid: Option<crate::economy::resource_catalog::ResourceId>,
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
            .insert_resource(JobEscrowIndex::default())
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
/// and partial completions release some workers earlier. Lifted from 600 → 1500
/// so a 20-person tribe in autumn can stockpile multiple weeks of food in one
/// posting instead of fragmenting the work into 60-tick re-postings.
const GATHER_TARGET_CAP: u32 = 1500;
const GATHER_TARGET_MIN: u32 = 80;

/// Item-count clamps for material (Wood/Stone) Gather postings. Lifted from
/// 32 → 96 so a 12-tile palisade run draws a single fat Stockpile posting
/// rather than three or four chained re-postings.
const MATERIAL_GATHER_MIN: u32 = 4;
const MATERIAL_GATHER_CAP: u32 = 96;

/// Farm posting target: number of tiles to plant in one posting.
const FARM_TILES_PER_POST: u32 = 6;

/// Chief job-posting reconciliation. Runs every `CHIEF_POSTING_INTERVAL` ticks
/// in `SimulationSet::Economy`. Posts Build jobs for non-personal blueprints,
/// Gather jobs when food per-head is low, Farm jobs when Agriculture is
/// researched and seeds/grain are low, and Craft jobs when supply<demand for
/// craftable goods. Reconciliation is idempotent: stale unclaimed Chief
/// postings whose target no longer needs work are dropped before new ones are
/// added.
/// Bundle of farm-planner inputs read by `chief_job_posting_system` so the
/// outer signature stays under Bevy's 16-param ceiling.
#[derive(bevy::ecs::system::SystemParam)]
pub struct FarmJobPostingParams<'w, 's> {
    pub assignments: Res<'w, crate::simulation::farm::FarmPlotAssignments>,
    pub plot_index: Res<'w, crate::simulation::land::PlotIndex>,
    pub plot_q: Query<'w, 's, &'static crate::simulation::land::Plot>,
    /// sleepy-dove Phase 6: classify build/haul postings by the
    /// authoring blueprint's poster (Architect vs Chief). Bundled here
    /// so `chief_job_posting_system` stays under the 16-param ceiling.
    pub poster_prof_q: Query<'w, 's, &'static crate::simulation::person::Profession>,
    /// Plot-level reachability gate for Farm postings (validated once per
    /// posting, not per tile). Bundled here to stay under the 16-param ceiling.
    pub chunk_map: Res<'w, crate::world::chunk::ChunkMap>,
    /// Seasonal farming jellyfish: per-tile nutrient + last-crop state,
    /// read by the chief Farm branch to classify each plot's tiles into
    /// `{unprepared, plantable, mature_grain}` buckets.
    pub field_tiles: Res<'w, crate::simulation::farm::FieldTileIndex>,
    /// Read to detect mature Grain plants in plot rects (Autumn harvest
    /// posting target).
    pub plant_map: Res<'w, crate::simulation::plants::PlantMap>,
    pub plant_q: Query<'w, 's, &'static crate::simulation::plants::Plant>,
}

/// Step 3: rebuild every faction's `procurement_plan` once per chief-posting
/// cadence. For each wall-ladder construction input it classifies scarcity via
/// `construction::classify_resource` (deposited storage, raw-gatherable
/// knowledge, market stock/price at the faction's node, treasury budget) and
/// records `HaulSource::Market` for inputs that are scarce-but-affordably-
/// procurable. `Storage`/absent inputs are left out (the default). Runs
/// `.before(chief_job_posting_system)` so Phase 3c reads a fresh plan.
pub fn classify_construction_procurement_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    shared: Res<crate::simulation::shared_knowledge::SharedKnowledge>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    camp_map: Res<crate::simulation::camp::CampMap>,
    settlement_q: Query<&crate::simulation::settlement::Settlement>,
    camp_q: Query<&crate::simulation::camp::Camp>,
) {
    if clock.tick % CHIEF_POSTING_INTERVAL != 0 {
        return;
    }
    use crate::economy::resource_catalog::ResourceId;
    use crate::simulation::camp::MarketNodeRef;
    use crate::simulation::construction::{
        classify_resource, recipe_for, BuildSiteKind, MaterialAvailabilityView, Scarcity,
        WallMaterial,
    };
    use crate::simulation::memory::MemoryKind;

    // Union of wall-ladder recipe inputs; per-rid `need` is the *largest*
    // single-structure input across the ladder (conservative — a faction with
    // 3 stone still reads short against the 4-stone Cut Stone rung).
    let mut need_by_rid: AHashMap<ResourceId, u32> = AHashMap::new();
    for m in WallMaterial::ALL {
        for &(rid, qty) in &recipe_for(BuildSiteKind::Wall(m)).inputs {
            let e = need_by_rid.entry(rid).or_insert(0);
            *e = (*e).max(qty as u32);
        }
    }
    if need_by_rid.is_empty() {
        return;
    }

    for (&faction_id, faction) in registry.factions.iter_mut() {
        faction.procurement_plan.clear();
        faction.procurement_market = None;
        let mut view = MaterialAvailabilityView::default();
        if faction_id == SOLO {
            faction.material_view = view;
            continue;
        }
        let node =
            crate::simulation::camp::faction_market_node(&settlement_map, &camp_map, faction_id);
        // Cache (node_entity, market_tile) so the Market-haul dispatcher can
        // route without resolving the node itself.
        faction.procurement_market = match node {
            Some(MarketNodeRef::Settlement(e)) => {
                settlement_q.get(e).ok().map(|s| (e, s.market_tile))
            }
            Some(MarketNodeRef::Camp(e)) => camp_q.get(e).ok().map(|c| (e, c.home_tile)),
            None => None,
        };
        let home = (faction.home_tile.0 as i32, faction.home_tile.1 as i32);
        let treasury = faction.treasury;
        for (&rid, &need) in need_by_rid.iter() {
            let stored = faction.storage.stock_of(rid);
            let supply = faction.resource_supply.get(&rid).copied().unwrap_or(stored);
            let raw_gatherable = crate::simulation::shared_knowledge::faction_knows_cluster(
                &shared,
                &settlement_map,
                faction_id,
                MemoryKind::Resource(rid),
                home,
                16,
            );
            let (mkt_stock, mkt_price) = match node {
                Some(MarketNodeRef::Settlement(e)) => settlement_q
                    .get(e)
                    .map(|s| (s.market.stock_of(rid), s.market.price_of(rid)))
                    .unwrap_or((0.0, 0.0)),
                Some(MarketNodeRef::Camp(e)) => camp_q
                    .get(e)
                    .map(|c| (c.market.stock_of(rid), c.market.price_of(rid)))
                    .unwrap_or((0.0, 0.0)),
                None => (0.0, 0.0),
            };
            let av = classify_resource(
                stored,
                supply,
                mkt_stock,
                mkt_price,
                treasury,
                raw_gatherable,
                need,
            );
            if av.scarcity == Scarcity::Scarce {
                faction.procurement_plan.insert(
                    rid,
                    HaulSource::Market {
                        max_unit_price: av.market_price,
                    },
                );
            }
            view.insert(rid, av);
        }
        faction.material_view = view;
    }
}

pub fn chief_job_posting_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    workbench_map: Res<crate::simulation::construction::WorkbenchMap>,
    loom_map: Res<crate::simulation::construction::LoomMap>,
    co_map: Res<crate::simulation::crafting::CraftOrderMap>,
    co_query: Query<&crate::simulation::crafting::CraftOrder>,
    projects: Res<Projects>,
    shared: Res<crate::simulation::shared_knowledge::SharedKnowledge>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    calendar: Res<crate::world::seasons::Calendar>,
    farm_params: FarmJobPostingParams,
    mut board: ResMut<JobBoard>,
    mut completed_events: EventWriter<JobCompletedEvent>,
) {
    if clock.tick % CHIEF_POSTING_INTERVAL != 0 {
        return;
    }
    // Resolve the `poster_class` for a blueprint-backed posting from the
    // blueprint's snapshotted `posted_by`. Architect-authored work
    // carries `PosterClass::Architect`; everything else (chief, seed,
    // legacy author-less) stays `Chief`.
    let poster_class_for_bp = |bp_e: Entity| -> PosterClass {
        bp_query
            .get(bp_e)
            .ok()
            .and_then(|b| b.posted_by)
            .and_then(|e| farm_params.poster_prof_q.get(e).ok())
            .map(|p| {
                if *p == crate::simulation::person::Profession::Architect {
                    PosterClass::Architect
                } else {
                    PosterClass::Chief
                }
            })
            .unwrap_or(PosterClass::Chief)
    };
    // Anticipatory food buffer: tribes need a fatter reserve heading into
    // winter than they do at the height of summer foraging. Multiplier is
    // applied to the food deficit target before the GATHER_TARGET_CAP clamp.
    let food_seasonal_multiplier: f32 = match calendar.season {
        crate::world::seasons::Season::Spring => 1.0,
        crate::world::seasons::Season::Summer => 1.0,
        crate::world::seasons::Season::Autumn => 1.5,
        crate::world::seasons::Season::Winter => 1.3,
    };

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
        // Nomadic factions don't post Stockpile/Farm/Build/Craft/Haul jobs:
        // they have no FactionStorageTile to deposit into, no plots to farm,
        // and Phase 7's `nomad_chief_directives` will own their slim build
        // menu. Members work via autonomous need-driven goals until then.
        // Capability check: archetypes with no posting layer skip chief postings.
        if faction.caps.posting.is_disabled() {
            continue;
        }
        // Phase C: Packed (mobile) bands skip all chief postings.
        if matches!(
            faction.camp_state,
            crate::simulation::faction::CampState::Packed { .. }
        ) {
            continue;
        }
        let live_bps: Vec<Entity> = bps_by_faction.get(&faction_id).cloned().unwrap_or_default();

        // 1. Drop stale unclaimed Chief postings whose target no longer needs work.
        //    Build postings whose project is not in the Build phase are also
        //    dropped here — the chief re-posts them once materials are in.
        //    Haul postings whose target blueprint despawned OR whose slot is
        //    already satisfied are dropped (Fix 1b — periodic catch-up for
        //    Fix 1a's per-tick eager cleanup at deposit time). Haul postings
        //    with claimants are NOT short-circuited: a satisfied-slot posting
        //    must drop and release its claimants regardless of claim status,
        //    otherwise haulers thrash in withdraw-walk-noop loops.
        {
            // Two-pass: pre-collect Haul postings to drop with claimant
            // cleanup, then run the standard retain on the rest. Mirrors the
            // pattern in `job_claim_release_system`.
            let postings = board.faction_postings_mut(faction_id);
            let mut to_drop_with_claimants: Vec<(
                JobId,
                JobKind,
                Vec<Entity>,
                Option<crate::economy::resource_catalog::ResourceId>,
            )> = Vec::new();
            for p in postings.iter() {
                if !matches!(p.source, JobSource::Chief) {
                    continue;
                }
                if let JobProgress::Haul {
                    blueprint,
                    resource_id,
                    ..
                } = p.progress
                {
                    let drop = match bp_query.get(blueprint) {
                        Err(_) => true,
                        Ok(bp) => bp.slot_satisfied(resource_id),
                    };
                    if drop {
                        to_drop_with_claimants.push((
                            p.id,
                            p.kind,
                            p.claimants.clone(),
                            p.progress.target_rid(),
                        ));
                    }
                }
            }
            for (job_id, kind, claimants, target_rid) in to_drop_with_claimants {
                if let Some(idx) = postings.iter().position(|p| p.id == job_id) {
                    postings.swap_remove(idx);
                }
                for c in &claimants {
                    commands.entity(*c).remove::<JobClaim>();
                    commands.entity(*c).remove::<ClaimTarget>();
                }
                // Phase 0: Haul-slot satisfied = genuine work
                // completion; the payout system pays claimants from
                // the escrow if any.
                completed_events.send(JobCompletedEvent {
                    job_id,
                    faction_id,
                    kind,
                    claimants,
                    completed: true,
                    target_rid,
                });
            }

            // Two-pass: identify chief postings to drop, emit a
            // `JobCompletedEvent { completed: false }` for each so
            // `job_payout_system` can despawn any funded escrow (the
            // `on_remove` hook then refunds wage + purchase_pool to the
            // beneficiary). Mirrors the Haul-drop pattern above so silent
            // posting drops can't orphan `JobEscrow` entities or strand
            // funds in `JobEscrowIndex`.
            let mut to_drop_silent: Vec<(
                JobId,
                JobKind,
                Vec<Entity>,
                Option<crate::economy::resource_catalog::ResourceId>,
            )> = Vec::new();
            for p in postings.iter() {
                if !matches!(p.source, JobSource::Chief) {
                    continue;
                }
                if !p.claimants.is_empty() {
                    continue;
                }
                let drop = match p.progress {
                    JobProgress::Building { blueprint } => {
                        if bp_query.get(blueprint).is_err() {
                            true
                        } else {
                            match projects.for_blueprint(blueprint) {
                                Some(project) => project.phase != ProjectPhase::Build,
                                None => true,
                            }
                        }
                    }
                    // Haul postings are already handled by the two-pass above.
                    JobProgress::Haul { .. } => false,
                    JobProgress::Calories { .. }
                    | JobProgress::Stockpile { .. }
                    | JobProgress::FieldWork { .. }
                    | JobProgress::Crafting { .. }
                    | JobProgress::Plow { .. } => true,
                };
                if drop {
                    to_drop_silent.push((
                        p.id,
                        p.kind,
                        p.claimants.clone(),
                        p.progress.target_rid(),
                    ));
                }
            }
            for (job_id, kind, claimants, target_rid) in to_drop_silent {
                if let Some(idx) = postings.iter().position(|p| p.id == job_id) {
                    postings.swap_remove(idx);
                }
                // Defensive: non-Haul drops gate on `claimants.is_empty()`
                // so this is a no-op today, but symmetric with the Haul
                // path keeps future predicate changes safe.
                for c in &claimants {
                    commands.entity(*c).remove::<JobClaim>();
                    commands.entity(*c).remove::<ClaimTarget>();
                }
                completed_events.send(JobCompletedEvent {
                    job_id,
                    faction_id,
                    kind,
                    claimants,
                    completed: false,
                    target_rid,
                });
            }
        }

        // 1b. Refresh priorities on all chief-source postings still alive so
        //     they track changing faction state without us having to drop and
        //     re-post.
        for p in board.faction_postings_mut(faction_id).iter_mut() {
            if matches!(p.source, JobSource::Chief) {
                p.priority = compute_priority(faction, faction_id, p.kind, &p.progress, &projects, &calendar);
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
                    !postings.iter().any(|p| {
                        matches!(
                            p.progress,
                            JobProgress::Building { blueprint } if blueprint == *bp_entity
                        )
                    })
                })
                .collect()
        };
        for bp_entity in needed_builds {
            let id = board.alloc_id();
            let progress = JobProgress::Building {
                blueprint: bp_entity,
            };
            let priority =
                compute_priority(faction, faction_id, JobKind::Build, &progress, &projects, &calendar);
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
                poster_class: poster_class_for_bp(bp_entity),
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
                // Phase 8: gate on faction-tier cluster knowledge. If no
                // edible cluster is known anywhere near home, the chief skips
                // the communal gather posting — local scarcity surfaces as a
                // gap that traders fill (R10) instead of futile foraging.
                let knows_food = crate::simulation::shared_knowledge::faction_knows_cluster(
                    &shared,
                    &settlement_map,
                    faction_id,
                    crate::simulation::memory::MemoryKind::AnyEdible,
                    (faction.home_tile.0 as i32, faction.home_tile.1 as i32),
                    16,
                );
                if food_total < target_supply && knows_food {
                    let deficit_units = target_supply.saturating_sub(food_total);
                    // Convert deficit units to a calorie target. Use the catalog's
                    // minimum edible-calorie value as a conservative baseline so
                    // adding richer foods (e.g. Fish, Cheese) doesn't change the
                    // target — deposits credit their actual nutrition, just
                    // finishing the posting faster.
                    let calories = deficit_units * crate::economy::core_ids::min_edible_calories();
                    let scaled = (calories as f32 * food_seasonal_multiplier) as u32;
                    let target = scaled.clamp(GATHER_TARGET_MIN, GATHER_TARGET_CAP);
                    let id = board.alloc_id();
                    let progress = JobProgress::Calories {
                        deposited: 0,
                        target,
                    };
                    let priority = compute_priority(
                        faction,
                        faction_id,
                        JobKind::Stockpile,
                        &progress,
                        &projects,
                        &calendar,
                    );
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
                // Phase 8: gate on faction-tier cluster knowledge. No known
                // wood/stone cluster within reach ⇒ skip the posting (real
                // local scarcity, not a labor allocation problem).
                if !crate::simulation::shared_knowledge::faction_knows_cluster(
                    &shared,
                    &settlement_map,
                    faction_id,
                    crate::simulation::memory::MemoryKind::Resource(target_rid),
                    (faction.home_tile.0 as i32, faction.home_tile.1 as i32),
                    16,
                ) {
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
                            bp_demand = bp_demand.saturating_add(
                                (slot.needed.saturating_sub(slot.deposited)) as u32,
                            );
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
                let priority = compute_priority(
                    faction,
                    faction_id,
                    JobKind::Stockpile,
                    &progress,
                    &projects,
                    &calendar,
                );
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
            let mut co_demand: AHashMap<crate::economy::resource_catalog::ResourceId, u32> =
                AHashMap::new();
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
                    *co_demand.entry(slot.resource_id).or_insert(0) = co_demand
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
                let priority = compute_priority(
                    faction,
                    faction_id,
                    JobKind::Stockpile,
                    &progress,
                    &projects,
                    &calendar,
                );
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
            let mut storage_remaining: AHashMap<crate::economy::resource_catalog::ResourceId, u32> =
                faction.storage.totals.clone();
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
                        // Storage can't cover this slot. If the classifier
                        // flagged the resource scarce-but-procurable AND this
                        // is a state-funded public-works blueprint authored by
                        // the chief / an Architect, post a treasury-funded
                        // Market haul instead of stalling. (Inert until the
                        // worker chain + escrow funding land in Steps 4-5;
                        // an unworked posting just refunds its escrow.)
                        if let Some(HaulSource::Market { max_unit_price }) =
                            faction.procurement_plan.get(&slot_id).copied()
                        {
                            let pc = poster_class_for_bp(bp_entity);
                            if faction.state_funds_public_works
                                && matches!(pc, PosterClass::Chief | PosterClass::Architect)
                            {
                                let id = board.alloc_id();
                                let progress = JobProgress::Haul {
                                    blueprint: bp_entity,
                                    resource_id: slot_id,
                                    delivered: 0,
                                    target: remaining,
                                    source: HaulSource::Market { max_unit_price },
                                };
                                let priority = compute_priority(
                                    faction,
                                    faction_id,
                                    JobKind::Haul,
                                    &progress,
                                    &projects,
                                    &calendar,
                                );
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
                                    poster_class: pc,
                                    reward: 0.0,
                                    settlement_id: None,
                                });
                            }
                        }
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
                        source: HaulSource::Storage,
                    };
                    let priority =
                        compute_priority(faction, faction_id, JobKind::Haul, &progress, &projects, &calendar);
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
                        poster_class: poster_class_for_bp(bp_entity),
                        reward: 0.0,
                        settlement_id: None,
                    });
                }
            }
        }

        // 4. Farm posting — seasonal field labor (jellyfish plan).
        // Dispatch on `farm_season_phase(&calendar)`:
        //   Spring     → Prepare (unworked tiles) + Plant (prepared,
        //                non-exhausted)
        //   Summer     → Low-priority caretaker Prepare for stragglers
        //   Autumn     → Harvest postings for mature Grain
        //   Winter     → No postings
        // Per-resource policy still gates: `chief_allocates_labor==false`
        // on Grain ⇒ private farmers run the cycle.
        let farm_chief_allocates = faction
            .policy_for(crate::economy::core_ids::grain())
            .chief_allocates_labor;
        if farm_chief_allocates
            && faction_can_perform(faction, JobKind::Farm)
            && faction.member_count > 0
        {
            let phase = crate::simulation::farm::farm_season_phase(&calendar);
            if !matches!(
                phase,
                crate::simulation::farm::FarmSeasonPhase::WinterDormant
            ) {
                let grain_seed_stock = faction.storage.seed_total();
                // Map of plots already covered by an open posting of a
                // particular phase, so we don't double-post.
                use crate::simulation::farm::FarmWorkPhase;
                let mut posted_by_phase: ahash::AHashMap<
                    (crate::simulation::land::PlotId, FarmWorkPhase),
                    (),
                > = ahash::AHashMap::default();
                // Seed budget shared across all Plant postings — subtract
                // remaining commitments on live Plant postings so a new plot
                // emit cannot overcommit the faction's seed pool. Each plot's
                // emit decrements `seed_remaining` so a multi-plot pass also
                // self-budgets.
                let mut seed_remaining: u32 = grain_seed_stock;
                for p in board.faction_postings(faction_id).iter() {
                    if !matches!(p.kind, JobKind::Farm) {
                        continue;
                    }
                    if let JobProgress::FieldWork {
                        phase,
                        plot_id,
                        target,
                        completed,
                        ..
                    } = p.progress
                    {
                        if let Some(pid) = plot_id {
                            posted_by_phase.insert((pid, phase), ());
                        }
                        if matches!(phase, FarmWorkPhase::Plant) {
                            let remaining_commit = target.saturating_sub(completed);
                            seed_remaining = seed_remaining.saturating_sub(remaining_commit);
                        }
                    }
                }

                // Enumerate all state-owned Ag plots for the faction.
                let plots: Vec<(
                    crate::simulation::land::PlotId,
                    crate::simulation::settlement::TileRect,
                )> = crate::simulation::farm::state_owned_ag_plots_for_faction(
                    faction_id,
                    &farm_params.plot_index,
                    &farm_params.plot_q,
                );

                for (pid, rect) in &plots {
                    let area = TileAabb {
                        min: (rect.x0, rect.y0),
                        max: (rect.x0 + rect.w as i32 - 1, rect.y0 + rect.h as i32 - 1),
                    };
                    if !crate::simulation::placement_reachability::rect_reachable_from_home(
                        &farm_params.chunk_map,
                        faction.home_tile,
                        area.min,
                        area.max,
                    ) {
                        continue;
                    }

                    // Tile-state classification. `mature_crop` counts any
                    // farm-plantable kind (`PlantKind::is_farm_plantable()`)
                    // at Mature inside the plot — so a Berry bush growing in
                    // an Agricultural plot creates an Autumn Harvest posting
                    // alongside Grain.
                    let mut unprepared = 0u32;
                    let mut plantable = 0u32;
                    let mut mature_crop = 0u32;
                    for ty in rect.y0..rect.y0 + rect.h as i32 {
                        for tx in rect.x0..rect.x0 + rect.w as i32 {
                            let kind_opt = farm_params.chunk_map.tile_kind_at(tx, ty);
                            let state = farm_params.field_tiles.by_tile.get(&(tx, ty));
                            let is_cropland =
                                matches!(kind_opt, Some(crate::world::tile::TileKind::Cropland));
                            let nutrients = state.map(|s| s.nutrients).unwrap_or(0);
                            let exhausted = nutrients < crate::simulation::farm::EXHAUSTED_FLOOR;
                            if !is_cropland || exhausted {
                                unprepared += 1;
                            }
                            if is_cropland
                                && nutrients >= crate::simulation::farm::MIN_PLANTABLE_NUTRIENTS
                            {
                                if let Some(pent) = farm_params.plant_map.0.get(&(tx, ty)) {
                                    if let Ok(pl) = farm_params.plant_q.get(*pent) {
                                        if pl.kind.is_farm_plantable()
                                            && pl.stage
                                                == crate::simulation::plants::GrowthStage::Mature
                                        {
                                            mature_crop += 1;
                                        }
                                    }
                                } else {
                                    plantable += 1;
                                }
                            }
                            // Mature-crop check on Cropland tiles below the
                            // plantable threshold (eg. tile yield dropped
                            // mid-season but the standing crop is still
                            // harvestable).
                            if is_cropland
                                && nutrients < crate::simulation::farm::MIN_PLANTABLE_NUTRIENTS
                            {
                                if let Some(pent) = farm_params.plant_map.0.get(&(tx, ty)) {
                                    if let Ok(pl) = farm_params.plant_q.get(*pent) {
                                        if pl.kind.is_farm_plantable()
                                            && pl.stage
                                                == crate::simulation::plants::GrowthStage::Mature
                                        {
                                            mature_crop += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Per-phase emission.
                    let mut emit = |progress: JobProgress| {
                        let id = board.alloc_id();
                        let priority = compute_priority(
                            faction,
                            faction_id,
                            JobKind::Farm,
                            &progress,
                            &projects,
                            &calendar,
                        );
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
                    };
                    let caretaker = farm_params.assignments.assigned_farmer(*pid);

                    match phase {
                        crate::simulation::farm::FarmSeasonPhase::SpringPrepPlant => {
                            // Prepare (any worker — assigned_farmer:None).
                            if unprepared > 0
                                && !posted_by_phase.contains_key(&(*pid, FarmWorkPhase::Prepare))
                            {
                                emit(JobProgress::FieldWork {
                                    phase: FarmWorkPhase::Prepare,
                                    completed: 0,
                                    target: unprepared,
                                    area,
                                    plot_id: Some(*pid),
                                    assigned_farmer: None,
                                });
                            }
                            // Plant (capped by remaining seed budget shared
                            // across this faction's plots).
                            let plant_target = plantable.min(seed_remaining);
                            if plant_target > 0
                                && !posted_by_phase.contains_key(&(*pid, FarmWorkPhase::Plant))
                            {
                                emit(JobProgress::FieldWork {
                                    phase: FarmWorkPhase::Plant,
                                    completed: 0,
                                    target: plant_target,
                                    area,
                                    plot_id: Some(*pid),
                                    assigned_farmer: None,
                                });
                                seed_remaining = seed_remaining.saturating_sub(plant_target);
                            }
                        }
                        crate::simulation::farm::FarmSeasonPhase::SummerMaintenance => {
                            // Caretaker-only Prepare for tiles that didn't
                            // make it into the Spring rush. Skip the emit when
                            // no caretaker is assigned — an open Summer
                            // Prepare posting is seasonally invalid and would
                            // be expired by `fieldwork_expiry_system` anyway.
                            if unprepared > 0
                                && caretaker.is_some()
                                && !posted_by_phase.contains_key(&(*pid, FarmWorkPhase::Prepare))
                            {
                                emit(JobProgress::FieldWork {
                                    phase: FarmWorkPhase::Prepare,
                                    completed: 0,
                                    target: unprepared,
                                    area,
                                    plot_id: Some(*pid),
                                    assigned_farmer: caretaker,
                                });
                            }
                        }
                        crate::simulation::farm::FarmSeasonPhase::AutumnHarvest => {
                            if mature_crop > 0
                                && !posted_by_phase.contains_key(&(*pid, FarmWorkPhase::Harvest))
                            {
                                emit(JobProgress::FieldWork {
                                    phase: FarmWorkPhase::Harvest,
                                    completed: 0,
                                    target: mature_crop,
                                    area,
                                    plot_id: Some(*pid),
                                    assigned_farmer: None,
                                });
                            }
                        }
                        crate::simulation::farm::FarmSeasonPhase::WinterDormant => {}
                    }
                }
                let _ = grain_seed_stock; // silence unused if no plots
                let _ = seed_remaining;
            }
        }

        // 4b. Plow posting — Draftwork v2. Spring-only. Per state-owned
        // Agricultural plot whose `plowed_year != Some(current_year)`, post
        // one `JobKind::Plow` if the faction (a) has `ARD_PLOW` tech, (b)
        // owns at least one `ard_plow` implement in storage, (c) the plot
        // hasn't already got a live Plow posting. `assigned_worker` is the
        // plot's `FarmPlotAssignments` farmer when present, else `None`
        // (open claim — any Farmer can pick it up).
        //
        // The dispatcher picks the animal at first-tile dispatch (gates on
        // species ∈ {Cattle, Horse} and `training >= TRAINING_THRESHOLD_DRAFT`).
        // The plow implement is NOT consumed by the executor — one implement
        // serves many seasons.
        if faction.techs.has(crate::simulation::technology::ARD_PLOW)
            && faction
                .storage
                .stock_of(crate::economy::core_ids::ard_plow())
                > 0
            && faction.member_count > 0
            && matches!(calendar.season, crate::world::seasons::Season::Spring)
        {
            let cur_year = calendar.year as u16;
            let mut plow_already_posted: ahash::AHashSet<crate::simulation::land::PlotId> =
                ahash::AHashSet::default();
            for p in board.faction_postings(faction_id).iter() {
                if let JobProgress::Plow { plot_id, .. } = p.progress {
                    plow_already_posted.insert(plot_id);
                }
            }
            let plow_plots: Vec<(
                crate::simulation::land::PlotId,
                crate::simulation::settlement::TileRect,
            )> = crate::simulation::farm::state_owned_ag_plots_for_faction(
                faction_id,
                &farm_params.plot_index,
                &farm_params.plot_q,
            );
            for (pid, rect) in &plow_plots {
                if plow_already_posted.contains(pid) {
                    continue;
                }
                // Check `plowed_year` via the plot entity.
                let plow_done_this_year = farm_params
                    .plot_index
                    .by_id
                    .get(pid)
                    .and_then(|ent| farm_params.plot_q.get(*ent).ok())
                    .and_then(|plot| plot.plowed_year)
                    == Some(cur_year);
                if plow_done_this_year {
                    continue;
                }
                if !crate::simulation::placement_reachability::rect_reachable_from_home(
                    &farm_params.chunk_map,
                    faction.home_tile,
                    (rect.x0, rect.y0),
                    (rect.x0 + rect.w as i32 - 1, rect.y0 + rect.h as i32 - 1),
                ) {
                    continue;
                }
                let area = TileAabb {
                    min: (rect.x0, rect.y0),
                    max: (rect.x0 + rect.w as i32 - 1, rect.y0 + rect.h as i32 - 1),
                };
                let target_tiles = (rect.w as u32).saturating_mul(rect.h as u32);
                if target_tiles == 0 {
                    continue;
                }
                let assigned_worker = farm_params.assignments.assigned_farmer(*pid);
                let progress = JobProgress::Plow {
                    plot_id: *pid,
                    area,
                    plowed_tiles: 0,
                    target_tiles,
                    assigned_worker,
                    animal: None,
                };
                let id = board.alloc_id();
                let priority =
                    compute_priority(faction, faction_id, JobKind::Plow, &progress, &projects, &calendar);
                board.faction_postings_mut(faction_id).push(JobPosting {
                    id,
                    faction_id,
                    kind: JobKind::Plow,
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

                // Track the best ingredient-ready recipe (eligible for an
                // immediate Craft posting) and, in parallel, the best
                // ingredient-blocked recipe (used to drive pull-posting of the
                // missing Stockpile inputs).
                // `best` / `best_blocked` are ranked priority-major: the
                // `(priority, deficit)` lexicographic key keeps a hard combat
                // gate (Weapon/Armor/Shield) ahead of a comfort good (Cloth)
                // even when the comfort good has the larger raw deficit.
                let mut best: Option<(u8, u8, u32, Option<Entity>)> = None;
                let mut blocked_demand: AHashMap<
                    crate::economy::resource_catalog::ResourceId,
                    u32,
                > = AHashMap::new();
                let mut best_blocked_key: (u8, u32) = (0, 0);
                for (idx, recipe) in crate::simulation::crafting::craft_recipes()
                    .iter()
                    .enumerate()
                {
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
                        Some(crate::simulation::crafting::StationKind::Workbench) => match bench {
                            Some(e) => Some(e),
                            None => continue,
                        },
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
                    // Functional craft demand — a per-output netted deficit
                    // computed by `resource_demand_system` via
                    // `compute_craft_demand`. An absent key means the good has
                    // no functional consumer and is never autonomously crafted.
                    let deficit = faction
                        .craft_demand
                        .get(&recipe.output_resource)
                        .copied()
                        .unwrap_or(0);
                    if deficit == 0 {
                        continue;
                    }
                    // Ingredient gate uses **storage stock**, not
                    // `resource_supply` (which includes member inventories).
                    // Only deposited inputs can be withdrawn to the bench, so a
                    // hunter holding 1 Skin must not green-light a Bow craft.
                    let mut missing: Vec<(crate::economy::resource_catalog::ResourceId, u32)> =
                        Vec::new();
                    for &(id, qty) in recipe.inputs.iter() {
                        let stocked = faction.storage.stock_of(id);
                        if stocked < qty {
                            missing.push((id, qty - stocked));
                        }
                    }
                    let priority = craft_priority(recipe.output_resource);
                    if missing.is_empty() {
                        if best.map_or(true, |(_, bp, bd, _)| (priority, deficit) > (bp, bd)) {
                            best = Some((idx as u8, priority, deficit, bench_ref));
                        }
                    } else if (priority, deficit) > best_blocked_key {
                        // Track the highest-priority blocked recipe; its missing
                        // inputs become the chief's pull demand below.
                        best_blocked_key = (priority, deficit);
                        blocked_demand.clear();
                        for (id, qty) in missing {
                            blocked_demand.insert(id, qty);
                        }
                    }
                }

                if let Some((recipe_id, _priority, deficit, bench_ref)) = best {
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
                        compute_priority(faction, faction_id, JobKind::Craft, &progress, &projects, &calendar);
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
                } else if !blocked_demand.is_empty() {
                    // Pull-schedule: the chief wants to craft something but
                    // ingredients aren't deposited yet. Post a Stockpile
                    // posting for each missing input the chief allocates
                    // labour for. The next chief-posting cycle re-evaluates
                    // and emits the actual Craft once stocks land.
                    let wood_id = crate::economy::core_ids::wood();
                    let stone_id = crate::economy::core_ids::stone();
                    for (target_rid, qty_needed) in blocked_demand {
                        // Wood/Stone are already covered by the dedicated
                        // anticipatory branch (3b) so let that path own them.
                        if target_rid == wood_id || target_rid == stone_id {
                            continue;
                        }
                        // A missing input that is itself a recipe output (e.g.
                        // Tools) can't be gathered — `craft_demand` already
                        // carries its derived demand, so a Stockpile posting
                        // here would only ever stall. Skip craftable inputs.
                        if crate::simulation::crafting::craft_recipes()
                            .iter()
                            .any(|r| r.output_resource == target_rid)
                        {
                            continue;
                        }
                        if !faction.policy_for(target_rid).chief_allocates_labor {
                            continue;
                        }
                        let already = board.faction_postings(faction_id).iter().any(|p| {
                            matches!(
                                &p.progress,
                                JobProgress::Stockpile { resource_id, .. }
                                    if *resource_id == target_rid
                            )
                        });
                        if already {
                            continue;
                        }
                        let target = qty_needed.clamp(MATERIAL_GATHER_MIN, MATERIAL_GATHER_CAP);
                        let id = board.alloc_id();
                        let progress = JobProgress::Stockpile {
                            resource_id: target_rid,
                            deposited: 0,
                            target,
                        };
                        let priority = compute_priority(
                            faction,
                            faction_id,
                            JobKind::Stockpile,
                            &progress,
                            &projects,
                            &calendar,
                        );
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
        }
    }
}

/// Pure snapshot of one faction's functional craft needs — input to
/// [`compute_craft_demand`]. See `plans/...curried-dewdrop.md` (Weapon/Ard Plow)
/// and `plans/...purrfect-rose.md` (Armor/Shield/Cloth): autonomous crafting
/// requires a concrete functional deficit, never a population quota.
pub struct CraftDemandInputs {
    /// Hunters + active raid-party members with no weapon equipped, carried,
    /// or in inventory.
    pub unarmed_combatants: u32,
    /// Spare weapons available to arm someone: faction storage + in-flight
    /// weapon crafts/orders. Equipped/carried/inventory weapons are NOT
    /// counted here — they satisfy their holder (lowering `unarmed_combatants`),
    /// not a spare pool.
    pub spare_weapons: u32,
    /// Combatants with no armor equipped/carried/in inventory. Armor mitigates
    /// real combat damage (`combat.rs`), so an unarmored combatant is a genuine
    /// functional deficit.
    pub unarmored_combatants: u32,
    /// Combatants with no shield equipped/carried/in inventory.
    pub unshielded_combatants: u32,
    /// Members whose torso is bare (no cloth or armor equipped, no cloth in
    /// inventory). Drives Cloth demand only when `can_weave`.
    pub unclothed_members: u32,
    /// True when the faction is Aware of `LOOM_WEAVING` — the Cloth gate.
    pub can_weave: bool,
    /// Spare supply for each good (faction storage + in-flight crafts/orders).
    /// Equipped/carried items satisfy their holder, not this pool.
    pub spare_armor: u32,
    pub spare_shield: u32,
    pub spare_cloth: u32,
    /// True when the faction is Aware of `ARD_PLOW`, owns ≥1 state-owned
    /// Agricultural plot, and has no Ard Plow in storage or in flight.
    pub wants_ard_plow: bool,
    /// Spare Tools (storage + in-flight) — nets the derived Ard-Plow-ingredient
    /// demand only.
    pub spare_tools: u32,
    /// Tools the Ard Plow recipe consumes per unit.
    pub ard_plow_tools_input: u32,
    pub weapon: crate::economy::resource_catalog::ResourceId,
    pub armor: crate::economy::resource_catalog::ResourceId,
    pub shield: crate::economy::resource_catalog::ResourceId,
    pub cloth: crate::economy::resource_catalog::ResourceId,
    pub ard_plow: crate::economy::resource_catalog::ResourceId,
    pub tools: crate::economy::resource_catalog::ResourceId,
    /// Realistic Tool Overhaul: per-form tool deficits the faction needs
    /// crafted — `(tool resource_id, missing count)`. Computed by
    /// `compute_faction_tool_summary` from member `ToolKit`s + storage.
    /// Replaces the generic `tools` demand path for functional tools (the
    /// `tools` commodity stays only as the Ard-Plow recipe ingredient).
    pub tool_form_deficits: Vec<(crate::economy::resource_catalog::ResourceId, u32)>,
}

/// Realistic Tool Overhaul: per-faction tool inventory summary. Counts how
/// many tools of each [`ToolForm`] the faction holds (member `ToolKit`s +
/// faction-storage tool stacks) and computes the deficit against the era
/// baseline — at minimum one of every form the faction's tech unlocks, plus
/// roughly one core tool per two members so the gather/dig gates aren't
/// starved. Returns `(tool resource_id, missing count)` pairs.
pub fn compute_faction_tool_deficits(
    era: crate::simulation::technology::Era,
    member_count: u32,
    has_bone_tools: bool,
    has_fishing: bool,
    has_crop_cultivation: bool,
    have_by_form: &AHashMap<crate::simulation::tools::ToolForm, u32>,
) -> Vec<(crate::economy::resource_catalog::ResourceId, u32)> {
    use crate::simulation::tools::starting_tool_loadout;
    let mut out = Vec::new();
    // Reuse the seeding loadout as the "want" baseline — same era scaling.
    for (form, _tier, want) in starting_tool_loadout(
        era,
        member_count,
        has_bone_tools,
        has_fishing,
        has_crop_cultivation,
    ) {
        let have = have_by_form.get(&form).copied().unwrap_or(0);
        let deficit = want.saturating_sub(have);
        if deficit > 0 {
            out.push((form.resource_id(), deficit));
        }
    }
    out
}

/// Pure functional craft-demand computation. Returns a per-output netted
/// deficit map; an absent key means zero demand (that good is never
/// autonomously crafted). Modelled goods: Weapon (unarmed combatants), Armor /
/// Shield (combatants lacking combat mitigation), Cloth (bare-torso members
/// when the faction can weave), Ard Plow, and Tools-as-an-Ard-Plow-ingredient.
/// Luxury / cart-parts have no hard functional consumer, so autonomous posting
/// for them is still dropped.
pub fn compute_craft_demand(
    inp: &CraftDemandInputs,
) -> AHashMap<crate::economy::resource_catalog::ResourceId, u32> {
    let mut out: AHashMap<crate::economy::resource_catalog::ResourceId, u32> = AHashMap::default();

    let weapon_deficit = inp.unarmed_combatants.saturating_sub(inp.spare_weapons);
    if weapon_deficit > 0 {
        out.insert(inp.weapon, weapon_deficit);
    }

    let armor_deficit = inp.unarmored_combatants.saturating_sub(inp.spare_armor);
    if armor_deficit > 0 {
        out.insert(inp.armor, armor_deficit);
    }

    let shield_deficit = inp
        .unshielded_combatants
        .saturating_sub(inp.spare_shield);
    if shield_deficit > 0 {
        out.insert(inp.shield, shield_deficit);
    }

    if inp.can_weave {
        let cloth_deficit = inp.unclothed_members.saturating_sub(inp.spare_cloth);
        if cloth_deficit > 0 {
            out.insert(inp.cloth, cloth_deficit);
        }
    }

    if inp.wants_ard_plow {
        out.insert(inp.ard_plow, 1);
        // Derived-input demand: the Ard Plow recipe consumes Tools, which are
        // craftable (not gatherable). Emit just enough Tools demand to unblock
        // one plow — no speculative reserve.
        let tools_deficit = inp.ard_plow_tools_input.saturating_sub(inp.spare_tools);
        if tools_deficit > 0 {
            out.insert(inp.tools, tools_deficit);
        }
    }

    // Realistic Tool Overhaul: functional per-form tool demand. Replaces the
    // generic `tools` commodity demand for actual working tools.
    for (rid, deficit) in &inp.tool_form_deficits {
        if *deficit > 0 {
            out.insert(*rid, *deficit);
        }
    }

    out
}

/// Static posting-priority rank for a craft-demand output. The chief Craft
/// branch selects the highest `(priority, deficit)` recipe, so a hard combat
/// gate is always crafted before a comfort good even when the comfort good has
/// the larger raw deficit (e.g. a 20-member cloth deficit must not starve a
/// 5-hunter weapon deficit). Keyed by `ResourceId` because Tools and Ard Plow
/// share `ResourceClass::Tool`.
pub fn craft_priority(rid: crate::economy::resource_catalog::ResourceId) -> u8 {
    use crate::economy::core_ids;
    if rid == core_ids::weapon() {
        4
    } else if rid == core_ids::tools()
        || crate::simulation::tools::ToolForm::from_resource_id(rid).is_some()
    {
        // Functional tools (and the legacy `tools` ingredient) sit above Ard
        // Plow / armor — a faction with no Pick can't even gather stone, so
        // tools gate most other production. Below Weapon (hunt/raid gate).
        3
    } else if rid == core_ids::armor() || rid == core_ids::shield() {
        2
    } else if rid == core_ids::ard_plow() {
        1
    } else {
        // Cloth and anything else — comfort-tier, crafted last.
        0
    }
}

#[cfg(test)]
mod craft_demand_tests {
    use super::*;
    use crate::economy::resource_catalog::ResourceId;

    // Synthetic ids — `compute_craft_demand` only uses them as map keys, so
    // these need no catalog.
    const WEAPON: ResourceId = ResourceId(1);
    const ARD_PLOW: ResourceId = ResourceId(2);
    const TOOLS: ResourceId = ResourceId(3);
    const ARMOR: ResourceId = ResourceId(4);
    const SHIELD: ResourceId = ResourceId(5);
    const CLOTH: ResourceId = ResourceId(6);

    fn base() -> CraftDemandInputs {
        CraftDemandInputs {
            unarmed_combatants: 0,
            spare_weapons: 0,
            unarmored_combatants: 0,
            unshielded_combatants: 0,
            unclothed_members: 0,
            can_weave: false,
            spare_armor: 0,
            spare_shield: 0,
            spare_cloth: 0,
            wants_ard_plow: false,
            spare_tools: 0,
            ard_plow_tools_input: 1,
            weapon: WEAPON,
            armor: ARMOR,
            shield: SHIELD,
            cloth: CLOTH,
            ard_plow: ARD_PLOW,
            tools: TOOLS,
            tool_form_deficits: Vec::new(),
        }
    }

    #[test]
    fn population_alone_yields_no_demand() {
        assert!(compute_craft_demand(&base()).is_empty());
    }

    #[test]
    fn unarmed_combatants_drive_weapon_demand() {
        let mut inp = base();
        inp.unarmed_combatants = 4;
        assert_eq!(compute_craft_demand(&inp).get(&WEAPON).copied(), Some(4));
        // Spare weapons (storage + in-flight) net the deficit down.
        inp.spare_weapons = 1;
        assert_eq!(compute_craft_demand(&inp).get(&WEAPON).copied(), Some(3));
        inp.spare_weapons = 4;
        assert!(compute_craft_demand(&inp).get(&WEAPON).is_none());
        inp.spare_weapons = 9;
        assert!(compute_craft_demand(&inp).get(&WEAPON).is_none());
    }

    #[test]
    fn ard_plow_derives_its_tools_ingredient() {
        let mut inp = base();
        inp.wants_ard_plow = true;
        let out = compute_craft_demand(&inp);
        assert_eq!(out.get(&ARD_PLOW).copied(), Some(1));
        assert_eq!(out.get(&TOOLS).copied(), Some(1));
        // Tools already stocked ⇒ plow still demanded, no derived tools.
        inp.spare_tools = 1;
        let out = compute_craft_demand(&inp);
        assert_eq!(out.get(&ARD_PLOW).copied(), Some(1));
        assert!(out.get(&TOOLS).is_none());
    }

    #[test]
    fn no_plow_want_means_no_plow_or_tools() {
        let out = compute_craft_demand(&base());
        assert!(out.get(&ARD_PLOW).is_none());
        assert!(out.get(&TOOLS).is_none());
    }

    #[test]
    fn unarmored_combatants_drive_armor_demand() {
        let mut inp = base();
        inp.unarmored_combatants = 3;
        assert_eq!(compute_craft_demand(&inp).get(&ARMOR).copied(), Some(3));
        inp.spare_armor = 1;
        assert_eq!(compute_craft_demand(&inp).get(&ARMOR).copied(), Some(2));
        inp.spare_armor = 3;
        assert!(compute_craft_demand(&inp).get(&ARMOR).is_none());
    }

    #[test]
    fn unshielded_combatants_drive_shield_demand() {
        let mut inp = base();
        inp.unshielded_combatants = 2;
        assert_eq!(compute_craft_demand(&inp).get(&SHIELD).copied(), Some(2));
        inp.spare_shield = 2;
        assert!(compute_craft_demand(&inp).get(&SHIELD).is_none());
    }

    #[test]
    fn cloth_demand_gated_on_weaving_tech() {
        let mut inp = base();
        inp.unclothed_members = 5;
        // No `LOOM_WEAVING` ⇒ no cloth demand regardless of bare torsos.
        assert!(compute_craft_demand(&inp).get(&CLOTH).is_none());
        inp.can_weave = true;
        assert_eq!(compute_craft_demand(&inp).get(&CLOTH).copied(), Some(5));
        // Spare cloth (storage + in-flight) nets the deficit down.
        inp.spare_cloth = 2;
        assert_eq!(compute_craft_demand(&inp).get(&CLOTH).copied(), Some(3));
        inp.spare_cloth = 5;
        assert!(compute_craft_demand(&inp).get(&CLOTH).is_none());
    }

    #[test]
    fn craft_priority_orders_combat_before_comfort() {
        use crate::economy::core_ids;
        // Weapon (hard hunt/raid gate) > Tools > Armor = Shield > Ard Plow >
        // Cloth (comfort).
        assert!(craft_priority(core_ids::weapon()) > craft_priority(core_ids::tools()));
        assert!(craft_priority(core_ids::tools()) > craft_priority(core_ids::armor()));
        assert_eq!(
            craft_priority(core_ids::armor()),
            craft_priority(core_ids::shield())
        );
        assert!(craft_priority(core_ids::shield()) > craft_priority(core_ids::ard_plow()));
        assert!(craft_priority(core_ids::ard_plow()) > craft_priority(core_ids::cloth()));
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
    use crate::simulation::crafting::{
        craft_recipes, recipe_encodes_knowledge, RECIPE_CLAY_TABLET,
    };
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
        // Phase C: Packed bands skip tablet posting.
        if matches!(
            faction.camp_state,
            crate::simulation::faction::CampState::Packed { .. }
        ) {
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

/// Pluralist Economy R9 — distance discount per tile in `U_bid`'s
/// `C_action` term. Same magnitude as the legacy formula's
/// `dist * 0.001` so paid postings compete on a similar geographic
/// footprint to communal ones, but with reward as the dominant
/// signal.
pub const BID_DIST_DISCOUNT: f32 = 0.001;

/// R9 — wealth modifier in `U_bid`'s `E(R)` term. Poor agents
/// value $1 more than rich ones — captures the diminishing marginal
/// utility of currency. Linear schedule: floor 1.0, additional
/// `+0.5 / (currency + 50)` boost (so an agent with 0 currency gets
/// 1.0 + 0.5/50 = 1.01 ≈ same as 50; an agent with 200 gets
/// 1.0 + 0.5/250 ≈ 1.002 — flatter at higher wealth). The constant
/// is small on purpose: R9 doesn't want wealth modifiers to swamp
/// the absolute reward signal — that's R12's contract-pricing
/// territory.
pub fn wealth_modifier(currency: f32) -> f32 {
    let baseline = 1.0_f32;
    let boost = 0.5 / (currency.max(0.0) + 50.0);
    baseline + boost
}

/// Skill axis used when scoring a candidate job for a worker.
fn skill_for(kind: JobKind) -> SkillKind {
    match kind {
        JobKind::Stockpile => SkillKind::Farming,
        JobKind::Haul => SkillKind::Farming,
        JobKind::Farm => SkillKind::Farming,
        JobKind::Build => SkillKind::Building,
        JobKind::Craft => SkillKind::Crafting,
        JobKind::Plow => SkillKind::Farming,
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
/// Phase 5a (wage-aware-labor-market-v2): Crafter affinity bonus
/// applied to `U_bid` for paid Craft postings. Sized so a Crafter
/// outscores an equidistant generalist by ~3 currency units; large
/// enough to dominate routing-cost tiebreaks but not so large that
/// it crowds out skill-based ranking when wages are tight.
pub const CRAFTER_AFFINITY_BONUS: f32 = 3.0;

fn profession_bias(p: Profession, kind: JobKind) -> f32 {
    match (p, kind) {
        (Profession::Farmer, JobKind::Farm) => 0.5,
        (Profession::Farmer, JobKind::Stockpile) => 0.1,
        (Profession::Crafter, JobKind::Craft) => 0.5,
        (Profession::Crafter, JobKind::Stockpile) => 0.1,
        _ => 0.0,
    }
}

/// Distinguishing key for claim-cap accounting. Stockpile postings split by
/// the targeted resource so wood/stone caps are independent of food's;
/// `FieldWork` postings split by `FarmWorkPhase` so Spring Prepare can't lock
/// Plant out of the shared seasonal cap (the balanced-farming fix). All other
/// kinds (Haul, Build, Farm/Plow, Craft) cap as a single bucket per JobKind.
fn cap_bucket(
    p: &JobPosting,
) -> (
    JobKind,
    Option<crate::economy::resource_catalog::ResourceId>,
    Option<crate::simulation::farm::FarmWorkPhase>,
) {
    match (&p.kind, &p.progress) {
        (JobKind::Stockpile, JobProgress::Stockpile { resource_id, .. }) => {
            (JobKind::Stockpile, Some(*resource_id), None)
        }
        (JobKind::Stockpile, JobProgress::Calories { .. }) => (JobKind::Stockpile, None, None),
        (JobKind::Farm, JobProgress::FieldWork { phase, .. }) => {
            (JobKind::Farm, None, Some(*phase))
        }
        _ => (p.kind, None, None),
    }
}

/// Soft per-posting headcount target. Layered on top of the per-bucket budget
/// cap (`cap_bucket` × `bucket_share`) so workers spread across multiple
/// postings of the same kind instead of piling onto one. Examples:
/// - A 4-tile palisade run admits 4 builders; a single hearth admits 2.
/// - A Stockpile target of 24 stone admits up to 6 haulers; a target of 4
///   admits 2.
/// - Craft / Farm are intrinsically single-claimant per posting today.
fn posting_target_workers(p: &JobPosting) -> u32 {
    match (&p.kind, &p.progress) {
        (JobKind::Build, _) => 3,
        (JobKind::Stockpile, JobProgress::Stockpile { target, .. }) => {
            ((*target / 4).max(2)).min(6)
        }
        (JobKind::Stockpile, JobProgress::Calories { target, .. }) => {
            ((*target / 80).max(2)).min(8)
        }
        (JobKind::Haul, _) => 2,
        // Seasonal Farm postings: scale with target tile/plant count so
        // Spring prep (256-tile plot) gets up to 12 workers but a 1-tile
        // caretaker run stays at 3.
        (JobKind::Farm, JobProgress::FieldWork { target, .. }) => ((*target / 8).max(3)).min(12),
        (JobKind::Farm, _) => 3,
        (JobKind::Craft, _) => 1,
        // Plow is a single-farmer single-animal job — one worker pulls the
        // ox across the field. Multiple workers can't share the same plow.
        (JobKind::Plow, _) => 1,
        _ => 1,
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
    calendar: Res<crate::world::seasons::Calendar>,
    registry: Res<FactionRegistry>,
    mut board: ResMut<JobBoard>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    chunk_graph: Res<crate::pathfinding::chunk_graph::ChunkGraph>,
    chunk_router: Res<crate::pathfinding::chunk_router::ChunkRouter>,
    workers: Query<
        (
            Entity,
            &AgentGoal,
            &FactionMember,
            &PersonAI,
            &crate::simulation::typed_task::ActionQueue,
            &LodLevel,
            &Skills,
            &Personality,
            Option<&Profession>,
            &Transform,
            Option<&crate::simulation::knowledge::PersonKnowledge>,
            // Pluralist Economy R9: needed by `U_bid`'s wealth_modifier.
            &crate::economy::agent::EconomicAgent,
            // Phase 6 of wage-aware-labor-market-v2: per-agent
            // entrepreneurial multiplier on `expected_reward`. Optional
            // so agents spawned before Phase 6 don't crash the claim
            // pass; missing component falls back to the neutral 1.5×
            // median (matching `Disposition::default()`).
            Option<&crate::simulation::goal_scorers::Disposition>,
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
            Option<crate::simulation::farm::FarmWorkPhase>,
        ),
        u32,
    > = AHashMap::new();
    for (faction_id, postings) in board.postings.iter() {
        for p in postings.iter() {
            let (kind, rid, phase) = cap_bucket(p);
            *claim_counts
                .entry((*faction_id, kind, rid, phase))
                .or_insert(0) += p.claimants.len() as u32;
        }
    }

    for (
        worker,
        goal,
        member,
        ai,
        aq,
        lod,
        skills,
        personality,
        profession_opt,
        transform,
        knowledge_opt,
        agent_econ,
        disposition_opt,
    ) in workers.iter()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if is_maintenance_goal(*goal) {
            continue;
        }
        if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
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
        // Spring carryover predicate: when only one of Prepare/Plant has an
        // open posting, that phase absorbs the other's share so the full
        // Spring envelope (PLANT + PREPARE) is still usable. Computed once
        // per worker iteration (board state doesn't change mid-loop).
        let (any_open_plant, any_open_prepare) = {
            let mut p_plant = false;
            let mut p_prep = false;
            for q in postings.iter() {
                if let JobProgress::FieldWork {
                    phase,
                    assigned_farmer: None,
                    ..
                } = q.progress
                {
                    match phase {
                        crate::simulation::farm::FarmWorkPhase::Plant => p_plant = true,
                        crate::simulation::farm::FarmWorkPhase::Prepare => p_prep = true,
                        _ => {}
                    }
                }
            }
            (p_plant, p_prep)
        };
        for (idx, p) in postings.iter().enumerate() {
            // Cap: per-bucket allocation is the workforce budget share applied
            // to current member count, floored at 1 so even small factions
            // can keep one worker on each role. Stockpile postings cap by
            // Good so food can't crowd out wood/stone slots.
            let (kind, rid, phase) = cap_bucket(p);
            let share = bucket_share(&budget, kind, rid);
            let mut cap = ((share * faction.member_count as f32).round() as u32).max(1);
            // Phase-weighted seasonal claim floor: an open seasonal `FieldWork`
            // posting (`assigned_farmer == None` — Spring prep/plant, Autumn
            // harvest) pulls a phase-weighted fraction of the village onto
            // field work, overriding the normal Farm budget cap. Spring is
            // split per phase (Plant 0.30 / Prepare 0.35) so Prepare can't
            // lock Plant out of a shared cap; when only one Spring phase is
            // active, the other phase's share carries over so the full
            // Spring envelope (0.65) is still consumed. Suppressed under
            // acute food pressure so emergency foraging wins; Plow and
            // Summer-caretaker (`assigned_farmer == Some`) postings are
            // untouched (they don't match this pattern).
            if kind == JobKind::Farm {
                if let JobProgress::FieldWork {
                    phase: this_phase,
                    assigned_farmer: None,
                    ..
                } = p.progress
                {
                    if (food_pressure(faction) as f32) < CRITICAL_FOOD_TRIGGER {
                        let floor_share =
                            crate::simulation::farm::seasonal_field_work_floor_share(
                                calendar.season,
                                this_phase,
                                any_open_plant,
                                any_open_prepare,
                            );
                        if floor_share > 0.0 {
                            let floor = ((floor_share * faction.member_count as f32).ceil()
                                as u32)
                                .max(1);
                            cap = cap.max(floor);
                        }
                    }
                }
            }
            let count = claim_counts
                .get(&(faction_id, kind, rid, phase))
                .copied()
                .unwrap_or(0);
            if count >= cap {
                continue;
            }
            // Per-posting cap so workers spread across multiple Build / Stockpile
            // postings of the same kind instead of all piling onto one.
            if (p.claimants.len() as u32) >= posting_target_workers(p) {
                continue;
            }
            // Skip postings that completed but haven't been removed yet.
            if p.progress.is_complete() {
                continue;
            }
            // Farm-planner §9: plot-scoped Farm postings restrict claiming
            // to the assigned farmer entity. Anyone else fails this gate
            // and the post stays open until the assigned farmer picks it
            // up (or the assignment is released by the daily reconciler).
            if let JobProgress::FieldWork {
                assigned_farmer: Some(assigned),
                ..
            } = p.progress
            {
                if assigned != worker {
                    continue;
                }
            }
            // Draftwork v2: Plow postings mirror the assigned-farmer
            // restriction. The chief picks one farmer per plot at posting
            // time; only that farmer can claim.
            if let JobProgress::Plow {
                assigned_worker: Some(assigned),
                ..
            } = p.progress
            {
                if assigned != worker {
                    continue;
                }
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
            // Detour-aware (river-aware) walk cost from the worker to the
            // posting target: a job across a river costs the walk-around,
            // not the straight line, so a worker doesn't out-bid a closer
            // peer for a far-bank posting.
            let dist = match target_tile {
                Some((tx, ty)) => {
                    let est = crate::pathfinding::detour::DetourEstimator::new(
                        &chunk_router,
                        &chunk_graph,
                    );
                    let tz = chunk_map.nearest_standable_z(tx, ty, ai.current_z as i32) as i8;
                    est.tiles(worker_tile, ai.current_z, (tx, ty), tz) as f32
                }
                None => 0.0,
            };
            // Pluralist Economy R9: U_bid scoring for paid postings.
            // Postings with `reward == 0.0` (chief / legacy
            // communal-labor postings under default policy) keep
            // today's `priority + skill + bias - distance` formula.
            // Paid postings (R10+ household / bureaucrat / individual)
            // score on `U_bid = E(R) - C_action - C_opportunity`.
            //
            // The C_opportunity term is stubbed to 0.0 in R9 — proper
            // wiring (calling `score_method_with_history` against the
            // agent's other applicable methods) lands when R10+
            // method paths come online. Until then, paid postings
            // are scored purely on reward + walk cost.
            let score = if p.reward > 0.0 {
                let wealth_mod = wealth_modifier(agent_econ.currency);
                // Phase 6: entrepreneurial disposition acts as a
                // per-agent multiplier on `expected_reward`. Default
                // (median 128) → 1.5×; max-entrepreneurial → 2.0×;
                // cautious agents → 1.0×. Pulls income-seeking agents
                // toward paid postings without forcing every agent
                // into the same scoring band.
                let disp_mod = disposition_opt
                    .map(|d| d.earn_income_multiplier())
                    .unwrap_or(1.5);
                let expected_reward = p.reward * wealth_mod * disp_mod;
                let c_action = dist * BID_DIST_DISCOUNT;
                let c_opportunity = 0.0_f32; // R9 stub; R10+ wires this
                                             // Phase 5a: Crafter claiming a Craft posting outscores
                                             // an equidistant generalist via `CRAFTER_AFFINITY_BONUS`.
                                             // Matches the unpaid-path `profession_bias` shape but
                                             // sized for currency units (the unpaid path lives in
                                             // skill-norm units).
                let affinity_bonus = if matches!(profession, Profession::Crafter)
                    && matches!(p.kind, JobKind::Craft)
                {
                    CRAFTER_AFFINITY_BONUS
                } else {
                    0.0
                };
                // Phase 4a/6 regression guard: paid chief postings
                // (`reward = trade_base_value × qty × CHIEF_MARGIN
                // (0.5)`) carry only half the market value of an
                // equivalent household / individual contract — without
                // a priority bias, private postings would consistently
                // outscore them on `expected_reward` alone and chief
                // postings sit unclaimed. Chief postings post at
                // `priority = 200` vs. household `180` and individual
                // `100-160`; mirroring the unpaid path's `priority
                // × 0.01` term to the paid path lifts chief postings
                // by ~1.0 currency unit over household contracts,
                // which restores competitive parity when chief wage
                // ≈ household reward.
                let priority_bonus = (p.priority as f32) * 0.01;
                expected_reward + affinity_bonus + priority_bonus - c_action - c_opportunity
            } else {
                (p.priority as f32) * 0.01
                    + skill_norm
                    + personality_bias(*personality, p.kind)
                    + profession_bias(profession, p.kind)
                    - dist * 0.001
            };
            match best {
                Some((_, s)) if s >= score => {}
                _ => best = Some((idx, score)),
            }
        }

        let Some((idx, _)) = best else { continue };
        // Apply the claim: insert component, push claimant, bump cap counter.
        let postings = board.faction_postings_mut(faction_id);
        let posting = &mut postings[idx];
        let (kind, rid, phase) = cap_bucket(posting);
        posting.claimants.push(worker);
        *claim_counts
            .entry((faction_id, kind, rid, phase))
            .or_insert(0) += 1;
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
        JobProgress::FieldWork { area, .. } => Some((
            (area.min.0 as i32 + area.max.0 as i32) as i32 / 2,
            (area.min.1 as i32 + area.max.1 as i32) as i32 / 2,
        )),
        JobProgress::Crafting { .. } => None,
        JobProgress::Building { .. } => None,
        JobProgress::Plow { area, .. } => {
            Some(((area.min.0 + area.max.0) / 2, (area.min.1 + area.max.1) / 2))
        }
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
/// goals to the job kind. Survival-maintenance goals keep the claim and only
/// refresh `ClaimTarget`; true external crisis goals still release the claim.
///
/// Also refreshes the `ClaimTarget` companion component so plan resolvers
/// can route to the specific blueprint/good named in the claimed posting.
pub fn job_goal_lock_system(
    mut commands: Commands,
    mut board: ResMut<JobBoard>,
    mut workers: Query<(Entity, &mut AgentGoal, &JobClaim, Option<&mut ClaimTarget>)>,
) {
    for (worker, mut goal, claim, mut target_opt) in workers.iter_mut() {
        let target = board
            .get(claim.job_id)
            .map(posting_claim_target)
            .unwrap_or_default();
        if is_maintenance_goal(*goal) {
            match target_opt.as_mut() {
                Some(existing) => {
                    **existing = target;
                }
                None => {
                    commands.entity(worker).insert(target);
                }
            }
            continue;
        }

        let crisis = matches!(
            *goal,
            AgentGoal::Defend | AgentGoal::Raid | AgentGoal::Rescue
        );
        if crisis {
            commands.entity(worker).remove::<JobClaim>();
            commands.entity(worker).remove::<ClaimTarget>();
            release_claimant(&mut board, claim.job_id, worker);
            continue;
        }
        // Phase 5 contract: the equality guards below avoid writing through
        // `&mut AgentGoal` when the lock target matches the agent's current
        // goal. Without them every tick triggers `Changed<AgentGoal>` for
        // every JobClaim'd worker, which `record_abandoned_method_system`
        // then misreads as a chain of goal flips and biases each working
        // method into oblivion.
        let target = if let Some(p) = board.get(claim.job_id) {
            let new_goal = posting_goal(p);
            if *goal != new_goal {
                *goal = new_goal;
            }
            posting_claim_target(p)
        } else {
            let new_goal = claim.kind.to_goal();
            if *goal != new_goal {
                *goal = new_goal;
            }
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
/// blueprint and resource a hauler/builder should route to. `Calories`
/// postings yield `ClaimKind::AnyEdible` so the food dispatcher's gate
/// passes for any catalog edible. Other multi-step variants (Planting,
/// Crafting) yield `ClaimTarget::default()` — they don't drive a single
/// resource-routed dispatcher.
pub fn posting_claim_target(p: &JobPosting) -> ClaimTarget {
    match &p.progress {
        JobProgress::Stockpile { resource_id, .. } => ClaimTarget {
            blueprint: None,
            kind: ClaimKind::Specific(*resource_id),
            haul_source: None,
        },
        JobProgress::Haul {
            blueprint,
            resource_id,
            source,
            ..
        } => ClaimTarget {
            blueprint: Some(*blueprint),
            kind: ClaimKind::Specific(*resource_id),
            haul_source: Some(*source),
        },
        JobProgress::Building { blueprint } => ClaimTarget {
            blueprint: Some(*blueprint),
            kind: ClaimKind::None,
            haul_source: None,
        },
        JobProgress::Calories { .. } => ClaimTarget {
            blueprint: None,
            kind: ClaimKind::AnyEdible,
            haul_source: None,
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
        for c in &claimants {
            commands.entity(*c).remove::<JobClaim>();
            commands.entity(*c).remove::<ClaimTarget>();
        }
        // Phase 0: blueprint despawn = build finished. Pay the
        // claimants who were on the build.
        completed_events.send(JobCompletedEvent {
            job_id,
            faction_id,
            kind,
            claimants,
            completed: true,
            target_rid: None,
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
        JobProgress::FieldWork {
            completed: progress_completed,
            target,
            ..
        } => {
            *progress_completed = progress_completed.saturating_add(increment);
            if progress_completed >= target {
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
        JobProgress::Plow {
            plowed_tiles,
            target_tiles,
            ..
        } => {
            // Plow progress comes through `record_progress_filtered(JobKind::Plow,
            // None, 1)` calls from the executor on each tile finished.
            if resource_id.is_some() {
                return;
            }
            *plowed_tiles = plowed_tiles.saturating_add(increment);
            if plowed_tiles >= target_tiles {
                completed = true;
            }
        }
    }
    if completed {
        let job_id = posting.id;
        let faction_id = posting.faction_id;
        let kind = posting.kind;
        let target_rid = posting.progress.target_rid();
        let claimants: Vec<Entity> = std::mem::take(&mut posting.claimants);
        // Remove the posting now that it's done.
        if let Some((fid, idx)) = board.locate(job_id) {
            board.postings.get_mut(&fid).unwrap().swap_remove(idx);
        }
        for c in &claimants {
            commands.entity(*c).remove::<JobClaim>();
            commands.entity(*c).remove::<ClaimTarget>();
        }
        completed_events.send(JobCompletedEvent {
            job_id,
            faction_id,
            kind,
            claimants,
            completed: true,
            target_rid,
        });
    }
}

/// Public helper for callers that have a tile and want to know whether it
/// falls within a posting's farm area (used by the planter completion hook).
pub fn planting_area_contains(progress: &JobProgress, tile: (i32, i32)) -> bool {
    match progress {
        JobProgress::FieldWork { area, .. } => area.contains(tile),
        _ => false,
    }
}

/// Credit a `FieldWork` posting by `delta`, **gated on its `phase`**. Mirrors
/// `record_progress_filtered`'s completion + claim-release + `JobCompletedEvent`
/// path. The phase gate is the structural guard against cross-phase crediting:
/// a Prepare-phase executor passes `Prepare`, the planting executor passes
/// `Plant`, the harvest path passes `Harvest`, so a worker holding a claim on
/// a posting of a *different* seasonal phase can never advance it. No-op when
/// the posting is gone, isn't `FieldWork`, or carries a different phase.
pub fn record_fieldwork_progress(
    commands: &mut Commands,
    board: &mut JobBoard,
    completed_events: &mut EventWriter<JobCompletedEvent>,
    job_id: JobId,
    phase: crate::simulation::farm::FarmWorkPhase,
    delta: u32,
) {
    if delta == 0 {
        return;
    }
    let Some(posting) = board.get_mut(job_id) else {
        return;
    };
    let JobProgress::FieldWork {
        phase: posting_phase,
        completed,
        target,
        ..
    } = &mut posting.progress
    else {
        return;
    };
    if *posting_phase != phase {
        return;
    }
    *completed = completed.saturating_add(delta);
    if *completed < *target {
        return;
    }
    // Completion: drop the posting, release every claimant, fire the event.
    let job_id = posting.id;
    let faction_id = posting.faction_id;
    let kind = posting.kind;
    let target_rid = posting.progress.target_rid();
    let claimants: Vec<Entity> = std::mem::take(&mut posting.claimants);
    if let Some((fid, idx)) = board.locate(job_id) {
        board.postings.get_mut(&fid).unwrap().swap_remove(idx);
    }
    for c in &claimants {
        commands.entity(*c).remove::<JobClaim>();
        commands.entity(*c).remove::<ClaimTarget>();
    }
    completed_events.send(JobCompletedEvent {
        job_id,
        faction_id,
        kind,
        claimants,
        completed: true,
        target_rid,
    });
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
                    let claimants = posting.claimants.clone();
                    for c in &claimants {
                        commands.entity(*c).remove::<JobClaim>();
                        commands.entity(*c).remove::<ClaimTarget>();
                    }
                    // Phase 0: cancellation is NOT a successful
                    // completion — payout system despawns the escrow
                    // with `amount > 0`, refunding the poster via the
                    // on_remove hook.
                    completed_events.send(JobCompletedEvent {
                        job_id,
                        faction_id: fid,
                        kind: posting.kind,
                        claimants,
                        completed: false,
                        target_rid: posting.progress.target_rid(),
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
        (JobProgress::Building { blueprint: x }, JobProgress::Building { blueprint: y }) => x == y,
        (JobProgress::Crafting { recipe: rx, .. }, JobProgress::Crafting { recipe: ry, .. }) => {
            rx == ry
        }
        (JobProgress::FieldWork { area: ax, .. }, JobProgress::FieldWork { area: ay, .. }) => {
            ax == ay
        }
        (JobProgress::Calories { .. }, JobProgress::Calories { .. }) => true,
        (
            JobProgress::Stockpile {
                resource_id: rx, ..
            },
            JobProgress::Stockpile {
                resource_id: ry, ..
            },
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
        (JobProgress::Plow { plot_id: px, .. }, JobProgress::Plow { plot_id: py, .. }) => px == py,
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
    mut workers: Query<(
        Entity,
        &AgentGoal,
        &PersonAI,
        &crate::simulation::typed_task::ActionQueue,
        &mut JobClaim,
    )>,
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
            if let JobProgress::Crafting { bench: Some(b), .. } = p.progress {
                if bench_query.get(b).is_err() {
                    to_remove.push(p.id);
                }
            }
        }
    }
    for job_id in to_remove {
        if let Some((fid, idx)) = board.locate(job_id) {
            let posting = board.postings.get_mut(&fid).unwrap().swap_remove(idx);
            let claimants = posting.claimants.clone();
            for c in &claimants {
                commands.entity(*c).remove::<JobClaim>();
                commands.entity(*c).remove::<ClaimTarget>();
            }
            // Phase 0: expiry / bench-invalid is NOT a successful
            // completion — payout system refunds via escrow despawn.
            completed_events.send(JobCompletedEvent {
                job_id,
                faction_id: fid,
                kind: posting.kind,
                claimants,
                completed: false,
                target_rid: posting.progress.target_rid(),
            });
        }
    }

    // 2. Stuck-idle fail-count bump. Workers idle without a task held longer
    // than STUCK_FAIL_INTERVAL since the claim was posted have their
    // fail_count incremented; once they hit MAX_FAIL_COUNT the claim is
    // released so the worker can pick something else.
    for (worker, goal, _ai, aq, mut claim) in workers.iter_mut() {
        if is_maintenance_goal(*goal) {
            continue;
        }
        if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
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

#[cfg(test)]
mod posting_target_workers_tests {
    use super::*;
    use bevy::prelude::{App, Entity, Update};

    fn stub_posting(kind: JobKind, progress: JobProgress) -> JobPosting {
        JobPosting {
            id: 0,
            faction_id: 0,
            kind,
            progress,
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

    #[test]
    fn build_posting_admits_three_workers() {
        let p = stub_posting(
            JobKind::Build,
            JobProgress::Building {
                blueprint: Entity::from_raw(1),
            },
        );
        assert_eq!(posting_target_workers(&p), 3);
    }

    #[test]
    fn small_stockpile_admits_two_workers() {
        // target=4 → (4/4).max(2).min(6) = 2.
        let rid = crate::economy::core_ids::wood();
        let p = stub_posting(
            JobKind::Stockpile,
            JobProgress::Stockpile {
                resource_id: rid,
                deposited: 0,
                target: 4,
            },
        );
        assert_eq!(posting_target_workers(&p), 2);
    }

    #[test]
    fn large_stockpile_admits_six_workers() {
        // target=96 → (96/4).max(2).min(6) = 6.
        let rid = crate::economy::core_ids::stone();
        let p = stub_posting(
            JobKind::Stockpile,
            JobProgress::Stockpile {
                resource_id: rid,
                deposited: 0,
                target: 96,
            },
        );
        assert_eq!(posting_target_workers(&p), 6);
    }

    #[test]
    fn calorie_posting_scales_with_target() {
        // target=80 → (80/80).max(2).min(8) = 2; target=800 → 8 (clamp).
        let small = stub_posting(
            JobKind::Stockpile,
            JobProgress::Calories {
                deposited: 0,
                target: 80,
            },
        );
        let large = stub_posting(
            JobKind::Stockpile,
            JobProgress::Calories {
                deposited: 0,
                target: 800,
            },
        );
        assert_eq!(posting_target_workers(&small), 2);
        assert_eq!(posting_target_workers(&large), 8);
    }

    #[test]
    fn craft_posting_admits_one_worker() {
        let p = stub_posting(
            JobKind::Craft,
            JobProgress::Crafting {
                crafted: 0,
                target: 1,
                recipe: 0,
                bench: None,
                tech_payload: None,
            },
        );
        assert_eq!(posting_target_workers(&p), 1);
    }

    #[test]
    fn job_goal_lock_preserves_maintenance_goal_and_claim() {
        let mut app = App::new();
        app.insert_resource(JobBoard::default());
        app.add_systems(Update, job_goal_lock_system);

        let blueprint = Entity::from_raw(42);
        let job_id = 7;
        let faction_id = 3;
        let worker = app
            .world_mut()
            .spawn((
                AgentGoal::Sleep,
                JobClaim {
                    job_id,
                    faction_id,
                    kind: JobKind::Build,
                    posted_tick: 0,
                    fail_count: 0,
                },
            ))
            .id();

        {
            let mut board = app.world_mut().resource_mut::<JobBoard>();
            let mut posting = stub_posting(JobKind::Build, JobProgress::Building { blueprint });
            posting.id = job_id;
            posting.faction_id = faction_id;
            posting.claimants.push(worker);
            board.faction_postings_mut(faction_id).push(posting);
        }

        app.update();

        let worker_ref = app.world().entity(worker);
        assert_eq!(
            *worker_ref.get::<AgentGoal>().unwrap(),
            AgentGoal::Sleep,
            "maintenance goal should not be overwritten by claim lock"
        );
        assert!(
            worker_ref.get::<JobClaim>().is_some(),
            "maintenance should preserve the claim"
        );
        let target = worker_ref
            .get::<ClaimTarget>()
            .expect("claim target should still be refreshed during maintenance");
        assert_eq!(target.blueprint, Some(blueprint));
    }
}

#[cfg(test)]
mod self_post_wage_tests {
    use super::*;
    use crate::economy::resource_catalog::load_resource_catalog;

    /// P4 minimal: wage formula `trade_base_value * qty * 0.1` against
    /// the values authored in `assets/data/resources/core.ron`.
    /// Hard-coded against the RON to make balance changes audible.
    #[test]
    fn wood_wage_5cu_per_unit() {
        let cat = load_resource_catalog();
        crate::economy::core_ids::install_catalog(cat.clone());
        let wood = crate::economy::core_ids::wood();
        // wood.trade_base_value = 5; qty=10 → 5 * 10 * 0.1 = 5.0
        let wage = self_post_wage(&cat, wood, 10);
        assert!(
            (wage - 5.0).abs() < 1e-6,
            "wood wage should be 5.0 (5 * 10 * 0.1), got {wage}",
        );
    }

    #[test]
    fn stone_wage_8cu_per_unit() {
        let cat = load_resource_catalog();
        crate::economy::core_ids::install_catalog(cat.clone());
        let stone = crate::economy::core_ids::stone();
        // stone.trade_base_value = 8; qty=10 → 8 * 10 * 0.1 = 8.0
        let wage = self_post_wage(&cat, stone, 10);
        assert!(
            (wage - 8.0).abs() < 1e-6,
            "stone wage should be 8.0 (8 * 10 * 0.1), got {wage}",
        );
    }

    #[test]
    fn grain_wage_10cu_per_unit() {
        let cat = load_resource_catalog();
        crate::economy::core_ids::install_catalog(cat.clone());
        let grain = crate::economy::core_ids::grain();
        // grain.trade_base_value = 10; qty=10 → 10 * 10 * 0.1 = 10.0
        let wage = self_post_wage(&cat, grain, 10);
        assert!(
            (wage - 10.0).abs() < 1e-6,
            "grain wage should be 10.0 (10 * 10 * 0.1), got {wage}",
        );
    }

    /// Wage scales linearly with quantity — same rid at higher qty
    /// produces proportional wage.
    #[test]
    fn wage_scales_linearly_with_qty() {
        let cat = load_resource_catalog();
        crate::economy::core_ids::install_catalog(cat.clone());
        let wood = crate::economy::core_ids::wood();
        let w1 = self_post_wage(&cat, wood, 10);
        let w5 = self_post_wage(&cat, wood, 50);
        assert!(
            (w5 - 5.0 * w1).abs() < 1e-6,
            "5x qty → 5x wage; got {w5} vs {}",
            5.0 * w1
        );
    }
}
