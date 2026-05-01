//! Faction-level **Projects**: the layer above the flat `JobBoard` that
//! encodes dependencies between gather and build work.
//!
//! A `Project` wraps a single Blueprint with a phase state machine:
//!
//! ```text
//! GatherMaterials  →  Build  →  (blueprint despawn drops the project)
//! ```
//!
//! While `GatherMaterials` is active, only Material/Calorie postings tied to
//! the blueprint's missing inputs are exposed to workers — the Build posting
//! is suppressed. Once the blueprint's deposit slots are full
//! (`Blueprint::is_satisfied`), the project advances to `Build` and the Build
//! posting opens. This makes the previous failure mode — "chief posts a Build
//! job before any materials exist" — structurally impossible.
//!
//! This module also owns the **pressure model** that replaces the old
//! `CHIEF_PRIORITY` constant (Stage 1) and the **workforce budget** that
//! replaces the flat 50%-per-kind cap in `job_claim_system` (Stage 2).
//! Stage 3 adds stagnation/cancel logic on top of the same `Project` data.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::goods::Good;
use crate::simulation::construction::{Blueprint, BlueprintMap};
use crate::simulation::faction::FactionData;
use crate::simulation::jobs::{faction_can_perform, JobKind, JobProgress};
use crate::simulation::schedule::SimClock;

pub type ProjectId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectPhase {
    /// Blueprint has unsatisfied deposits. Material/Calorie postings are
    /// open; Build posting is suppressed.
    GatherMaterials,
    /// Blueprint deposits are full. Build posting is open.
    Build,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub id: ProjectId,
    pub faction_id: u32,
    pub blueprint: Entity,
    pub spawn_tick: u32,
    /// Last tick the blueprint reported any deposit progress. Stage 3 uses
    /// `now - last_progress_tick > STAGNATION_TICKS` to detect stalled gathers.
    pub last_progress_tick: u32,
    /// Snapshot of deposited totals from the previous lifecycle pass; used to
    /// detect "any new progress" without storing a history buffer.
    pub last_deposited_total: u32,
    pub phase: ProjectPhase,
}

/// Per-project lifecycle event surfaced to the Debug panel and Inspector so
/// players can see why a build was downgraded or cancelled.
#[derive(Clone, Debug)]
pub enum ProjectEventKind {
    Cancelled { reason: ProjectCancelReason },
}

#[derive(Clone, Copy, Debug)]
pub enum ProjectCancelReason {
    StalledGather { good: Good },
}

#[derive(Clone, Debug)]
pub struct ProjectEvent {
    pub tick: u32,
    pub faction_id: u32,
    pub blueprint: Entity,
    pub kind: ProjectEventKind,
}

#[derive(Resource, Default)]
pub struct Projects {
    pub projects: AHashMap<ProjectId, Project>,
    pub by_blueprint: AHashMap<Entity, ProjectId>,
    pub recent_events: Vec<ProjectEvent>,
    next_id: ProjectId,
}

impl Projects {
    const MAX_EVENTS: usize = 16;

    fn record_event(&mut self, event: ProjectEvent) {
        if self.recent_events.len() >= Self::MAX_EVENTS {
            self.recent_events.remove(0);
        }
        self.recent_events.push(event);
    }
}

impl Projects {
    fn alloc_id(&mut self) -> ProjectId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    pub fn for_blueprint(&self, blueprint: Entity) -> Option<&Project> {
        self.by_blueprint
            .get(&blueprint)
            .and_then(|id| self.projects.get(id))
    }

    pub fn faction_projects<'a>(
        &'a self,
        faction_id: u32,
    ) -> impl Iterator<Item = &'a Project> + 'a {
        self.projects
            .values()
            .filter(move |p| p.faction_id == faction_id)
    }
}

// ── Lifecycle system ─────────────────────────────────────────────────────────

/// Maintain the `Projects` resource: create projects for new blueprints,
/// advance phases when deposits fill, drop projects whose blueprint despawned.
/// Runs every tick in `Economy` ahead of `chief_job_posting_system` so the
/// posting reconciliation sees fresh phase state.
pub fn project_lifecycle_system(
    clock: Res<SimClock>,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    mut projects: ResMut<Projects>,
) {
    let now = clock.tick as u32;

    // Drop projects whose blueprint vanished (built or cancelled).
    let stale: Vec<ProjectId> = projects
        .projects
        .iter()
        .filter_map(|(&id, project)| {
            if bp_query.get(project.blueprint).is_err() {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    for id in stale {
        if let Some(project) = projects.projects.remove(&id) {
            projects.by_blueprint.remove(&project.blueprint);
        }
    }

    // Create projects for any unowned blueprints, and refresh phase + progress.
    for &bp_entity in bp_map.0.values() {
        let Ok(bp) = bp_query.get(bp_entity) else {
            continue;
        };
        // Personal commissions live outside faction job orchestration.
        if bp.personal_owner.is_some() {
            continue;
        }
        let deposited_total = blueprint_deposited_total(bp);
        let want_phase = if bp.is_satisfied() {
            ProjectPhase::Build
        } else {
            ProjectPhase::GatherMaterials
        };

        match projects.by_blueprint.get(&bp_entity).copied() {
            Some(pid) => {
                if let Some(project) = projects.projects.get_mut(&pid) {
                    if deposited_total > project.last_deposited_total {
                        project.last_progress_tick = now;
                        project.last_deposited_total = deposited_total;
                    }
                    project.phase = want_phase;
                }
            }
            None => {
                let id = projects.alloc_id();
                projects.projects.insert(
                    id,
                    Project {
                        id,
                        faction_id: bp.faction_id,
                        blueprint: bp_entity,
                        spawn_tick: now,
                        last_progress_tick: now,
                        last_deposited_total: deposited_total,
                        phase: want_phase,
                    },
                );
                projects.by_blueprint.insert(bp_entity, id);
            }
        }
    }
}

fn blueprint_deposited_total(bp: &Blueprint) -> u32 {
    let mut total = 0u32;
    for i in 0..bp.deposit_count as usize {
        total = total.saturating_add(bp.deposits[i].deposited as u32);
    }
    total
}

/// Sum the unmet input quantities for a blueprint, grouped by Good.
pub fn blueprint_remaining_inputs(bp: &Blueprint) -> AHashMap<Good, u32> {
    let mut out: AHashMap<Good, u32> = AHashMap::new();
    for i in 0..bp.deposit_count as usize {
        let slot = &bp.deposits[i];
        let remaining = (slot.needed.saturating_sub(slot.deposited)) as u32;
        if remaining > 0 {
            *out.entry(slot.good).or_insert(0) += remaining;
        }
    }
    out
}

// ── Pressure / priority model ─────────────────────────────────────────────────
//
// Priority is no longer a constant. Each posting's effective priority is
// `base_priority(kind) + pressure(faction, posting)` clamped to u8.
// Pressures are first-pass — the goal is "starvation overrides build, build
// blocked on stone overrides build blocked on nothing", not perfect tuning.

pub const PRIORITY_PLAYER: u8 = 220;

const BASE_GATHER_FOOD: u8 = 80;
const BASE_GATHER_MATERIAL: u8 = 60;
const BASE_BUILD: u8 = 40;
const BASE_FARM: u8 = 60;
const BASE_CRAFT: u8 = 50;

/// Per-head food stockpile target. Below this, food-pressure ramps.
const FOOD_PER_HEAD_TARGET: f32 = 8.0;

/// Compute the priority for a posting given faction state. Pure function —
/// reads `FactionData` and `Projects`, mutates nothing. Called both at posting
/// time and on each chief reconciliation tick so priorities track changing
/// state (food deficit deepening, material deficit clearing, etc).
pub fn compute_priority(
    faction: &FactionData,
    faction_id: u32,
    posting_kind: JobKind,
    progress: &JobProgress,
    projects: &Projects,
) -> u8 {
    let base = match posting_kind {
        JobKind::Gather => match progress {
            JobProgress::Material { .. } => BASE_GATHER_MATERIAL,
            _ => BASE_GATHER_FOOD,
        },
        JobKind::Build => BASE_BUILD,
        JobKind::Farm => BASE_FARM,
        JobKind::Craft => BASE_CRAFT,
    };

    let pressure: u8 = match (posting_kind, progress) {
        (JobKind::Gather, JobProgress::Calories { .. }) => food_pressure(faction),
        (JobKind::Gather, JobProgress::Material { good, .. }) => {
            material_pressure(faction, faction_id, projects, *good)
        }
        (JobKind::Build, JobProgress::Building { .. }) => build_pressure(),
        (JobKind::Farm, _) => farm_pressure(faction),
        (JobKind::Craft, _) => craft_pressure(faction),
        _ => 0,
    };

    base.saturating_add(pressure)
}

/// 0..=120: how badly does the faction need food right now? Scales with the
/// per-head food deficit; saturates as stocks approach empty.
pub fn food_pressure(faction: &FactionData) -> u8 {
    if faction.member_count == 0 {
        return 0;
    }
    let per_head = faction.storage.food_total() / faction.member_count as f32;
    if per_head >= FOOD_PER_HEAD_TARGET {
        return 0;
    }
    let deficit_ratio = ((FOOD_PER_HEAD_TARGET - per_head) / FOOD_PER_HEAD_TARGET).clamp(0.0, 1.0);
    (deficit_ratio * 120.0) as u8
}

/// 0..=80: build projects waiting on this material add pressure. Empty
/// storage of this good adds extra so chronic shortfalls climb fast.
pub fn material_pressure(
    faction: &FactionData,
    faction_id: u32,
    projects: &Projects,
    good: Good,
) -> u8 {
    let stored = faction.storage.totals.get(&good).copied().unwrap_or(0);
    let waiting = projects
        .faction_projects(faction_id)
        .filter(|p| p.phase == ProjectPhase::GatherMaterials)
        .count() as u32;
    let bump_per_project = if stored == 0 { 30 } else { 18 };
    waiting.saturating_mul(bump_per_project).min(80) as u8
}

pub fn build_pressure() -> u8 {
    // First-pass: a flat bump on Build phase keeps finishing-the-build a
    // priority once materials are in, without overpowering food/material.
    40
}

pub fn farm_pressure(faction: &FactionData) -> u8 {
    if !faction_can_perform(faction, JobKind::Farm) {
        return 0;
    }
    let grain = faction.storage.totals.get(&Good::Grain).copied().unwrap_or(0);
    let target = faction.member_count.saturating_mul(4);
    if grain >= target || target == 0 {
        return 0;
    }
    (target - grain).min(60) as u8
}

pub fn craft_pressure(faction: &FactionData) -> u8 {
    let supply = faction
        .resource_supply
        .get(&Good::Tools)
        .copied()
        .unwrap_or(0);
    let demand = faction
        .resource_demand
        .get(&Good::Tools)
        .copied()
        .unwrap_or(0);
    let gap = demand.saturating_sub(supply);
    (gap.saturating_mul(2)).min(60) as u8
}

// ── Workforce budget (Stage 2) ───────────────────────────────────────────────

/// Per-faction allocation across job kinds. Sums to 1.0. Computed from the
/// same pressures that drive priority, modulated by `FactionCulture`.
#[derive(Clone, Copy, Debug)]
pub struct WorkforceBudget {
    pub gather: f32,
    pub farm: f32,
    pub build: f32,
    pub craft: f32,
    pub free: f32,
}

impl Default for WorkforceBudget {
    fn default() -> Self {
        Self {
            gather: 0.30,
            farm: 0.15,
            build: 0.15,
            craft: 0.15,
            free: 0.25,
        }
    }
}

impl WorkforceBudget {
    pub fn share(&self, kind: JobKind) -> f32 {
        match kind {
            JobKind::Gather => self.gather,
            JobKind::Farm => self.farm,
            JobKind::Build => self.build,
            JobKind::Craft => self.craft,
        }
    }
}

const FREE_FLOOR: f32 = 0.10;
const BUDGET_EMA_ALPHA: f32 = 0.25;
const BUDGET_TEMPERATURE: f32 = 1.0;
const ROLE_FLOOR: f32 = 5.0;

/// Compute the next workforce budget from current pressures, blend with the
/// previous tick's via EMA so the budget doesn't whipsaw on noisy state.
pub fn compute_workforce_budget(
    faction: &FactionData,
    projects: &Projects,
    faction_id: u32,
    previous: WorkforceBudget,
) -> WorkforceBudget {
    let food = food_pressure(faction) as f32;
    let mut material = 0.0f32;
    let mut waiting_build = 0u32;
    for project in projects.faction_projects(faction_id) {
        match project.phase {
            ProjectPhase::GatherMaterials => material += 20.0,
            ProjectPhase::Build => waiting_build += 1,
        }
    }
    let farm = farm_pressure(faction) as f32;
    let craft = craft_pressure(faction) as f32;
    let build = waiting_build as f32 * 30.0;

    // Raw weights — gather absorbs both food and material pressure.
    let mut raw_gather = food + material;
    let mut raw_farm = farm;
    let mut raw_build = build;
    let mut raw_craft = craft;

    // Culture modulation. Trait values are 0..=255; map to ~0.5..=1.5×.
    let scale = |t: u8, k: f32| 1.0 + ((t as f32 / 255.0) - 0.5) * 2.0 * k;
    let c = &faction.culture;
    raw_build *= scale(c.defensive, 0.4);
    raw_craft *= scale(c.ceremonial, 0.3);
    raw_craft *= scale(c.mercantile, 0.3);
    raw_gather *= scale(c.mercantile, 0.2);
    raw_build *= scale(c.martial, 0.2);

    // Capability gate — kinds the faction can't yet perform are excluded from
    // the softmax entirely (NEG_INFINITY → exp() = 0). Skip the role floor for
    // those, otherwise it would resurrect them. Same predicate used by the
    // chief's posting code, so budget and postings can't drift.
    let kinds = [JobKind::Gather, JobKind::Farm, JobKind::Build, JobKind::Craft];
    let mut raw = [raw_gather, raw_farm, raw_build, raw_craft];
    for (i, kind) in kinds.iter().enumerate() {
        if faction_can_perform(faction, *kind) {
            raw[i] = raw[i].max(ROLE_FLOOR);
        } else {
            raw[i] = f32::NEG_INFINITY;
        }
    }

    // Softmax over modulated weights (numerically stable).
    let t = BUDGET_TEMPERATURE.max(0.01);
    let max_w = raw.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps = [
        ((raw[0] - max_w) / t).exp(),
        ((raw[1] - max_w) / t).exp(),
        ((raw[2] - max_w) / t).exp(),
        ((raw[3] - max_w) / t).exp(),
    ];
    let sum: f32 = exps.iter().sum::<f32>().max(1e-6);
    let usable = 1.0 - FREE_FLOOR;
    let target = WorkforceBudget {
        gather: (exps[0] / sum) * usable,
        farm: (exps[1] / sum) * usable,
        build: (exps[2] / sum) * usable,
        craft: (exps[3] / sum) * usable,
        free: FREE_FLOOR,
    };

    // EMA hysteresis with the previous budget.
    let blend = |old: f32, new: f32| old + (new - old) * BUDGET_EMA_ALPHA;
    WorkforceBudget {
        gather: blend(previous.gather, target.gather),
        farm: blend(previous.farm, target.farm),
        build: blend(previous.build, target.build),
        craft: blend(previous.craft, target.craft),
        free: blend(previous.free, target.free),
    }
}

// ── Stage 3: stagnation, cancellation, deficit EMA ───────────────────────────

/// Ticks of zero deposit progress in `GatherMaterials` after which we declare
/// the project stalled and cancel it. 600 ticks ≈ 30s at 20 Hz fixed update.
const STAGNATION_TICKS: u32 = 600;

/// EMA blend factor for the per-good material-deficit signal. Each chief
/// tick that ends with a stagnated material project bumps this toward 255;
/// successful gathers decay it toward 0.
const DEFICIT_EMA_ALPHA: f32 = 0.3;

/// `material_deficit_ema` value at or above which we treat the resource as
/// "rare in the territory" and suppress candidates that need it.
pub const DEFICIT_EMA_RARE_THRESHOLD: u8 = 160;

/// Detect stalled `GatherMaterials` projects and cancel them. Updates the
/// faction's `material_deficit_ema` for the stalled good so future
/// `generate_candidates` invocations avoid that input until supply recovers.
/// Runs in `Economy` after `project_lifecycle_system` so phase + progress are
/// fresh.
pub fn project_stagnation_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    bp_query: Query<&Blueprint>,
    mut bp_map: ResMut<BlueprintMap>,
    mut projects: ResMut<Projects>,
    mut registry: ResMut<crate::simulation::faction::FactionRegistry>,
) {
    let now = clock.tick as u32;

    // Cull projects that have stalled for too long. We collect cancellations
    // first to avoid mutating `projects` while iterating it.
    let mut to_cancel: Vec<(ProjectId, Entity, u32, Good, (i16, i16))> = Vec::new();
    for project in projects.projects.values() {
        if project.phase != ProjectPhase::GatherMaterials {
            continue;
        }
        if now.saturating_sub(project.last_progress_tick) < STAGNATION_TICKS {
            continue;
        }
        let Ok(bp) = bp_query.get(project.blueprint) else {
            continue;
        };
        // Pick the most-needed unmet good — the one likely blocking progress.
        let remaining = blueprint_remaining_inputs(bp);
        let Some((&good, _)) = remaining.iter().max_by_key(|(_, qty)| **qty) else {
            continue;
        };
        to_cancel.push((project.id, project.blueprint, project.faction_id, good, bp.tile));
    }

    for (project_id, blueprint, faction_id, good, tile) in to_cancel {
        // Bump the faction's deficit EMA for the stalled good.
        if let Some(faction) = registry.factions.get_mut(&faction_id) {
            let prev = faction.material_deficit_ema.get(&good).copied().unwrap_or(0) as f32;
            let next = (prev + (255.0 - prev) * DEFICIT_EMA_ALPHA).round() as u8;
            faction.material_deficit_ema.insert(good, next);
        }
        // Despawn the blueprint and unregister it from the BlueprintMap so
        // the chief's one-project-at-a-time gate releases.
        bp_map.0.remove(&tile);
        commands.entity(blueprint).despawn_recursive();
        projects.projects.remove(&project_id);
        projects.by_blueprint.remove(&blueprint);
        projects.record_event(ProjectEvent {
            tick: now,
            faction_id,
            blueprint,
            kind: ProjectEventKind::Cancelled {
                reason: ProjectCancelReason::StalledGather { good },
            },
        });
    }

    // Decay deficit EMAs slowly when no stagnation fires this tick. The decay
    // runs continuously so a one-time stall doesn't permanently blacklist a
    // material; recovery is gradual.
    if clock.tick % 60 == 0 {
        for faction in registry.factions.values_mut() {
            for value in faction.material_deficit_ema.values_mut() {
                let prev = *value as f32;
                let next = (prev * (1.0 - DEFICIT_EMA_ALPHA * 0.25)).round() as u8;
                *value = next;
            }
        }
    }
}

/// How often to recompute each faction's `WorkforceBudget`. Cheap pure
/// function — no need to run more often than the chief reconciles postings.
const BUDGET_RECOMPUTE_INTERVAL: u64 = 60;

/// Refresh `FactionData::workforce_budget` for every faction. Runs in
/// `Economy` after `compute_faction_storage_system` (so storage totals are
/// fresh) and after `project_lifecycle_system` (so phase counts are fresh).
/// Consumed by `job_claim_system` next frame as the per-kind cap.
pub fn workforce_budget_system(
    clock: Res<SimClock>,
    projects: Res<Projects>,
    mut registry: ResMut<crate::simulation::faction::FactionRegistry>,
) {
    if clock.tick % BUDGET_RECOMPUTE_INTERVAL != 0 {
        return;
    }
    for (&faction_id, faction) in registry.factions.iter_mut() {
        let next = compute_workforce_budget(faction, &projects, faction_id, faction.workforce_budget);
        faction.workforce_budget = next;
    }
}

// ── Plugin ───────────────────────────────────────────────────────────────────

pub struct ProjectsPlugin;

impl Plugin for ProjectsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Projects::default());
    }
}
