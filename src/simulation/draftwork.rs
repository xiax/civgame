//! Animal husbandry v2 — Draftwork (plow pipeline).
//!
//! This module turns domesticated cattle / horses into a real economic input
//! by wiring up the missing executor surface around `AnimalWorkClaim`. v2.0
//! ships the *plow* half end-to-end via the standard `JobBoard` + `JobClaim`
//! pipeline; carts follow in v2.1.
//!
//! ## End-to-end pipeline
//! 1. **Animal training (`animal_training_progress_system`)** — quarter-day
//!    cadence; housed `DomesticAnimal`s gain `+1` training per pass.
//!    Threshold `TRAINING_THRESHOLD_DRAFT = 80` gates draft eligibility.
//! 2. **Chief posting (`jobs::chief_job_posting_system` Plow branch)** —
//!    Spring-gated. Per state-owned Agricultural plot with
//!    `plowed_year != Some(current_year)`, posts one `JobKind::Plow` with
//!    `assigned_worker = FarmPlotAssignments.assigned_farmer(plot)`,
//!    `area = plot.rect`, `target_tiles = plot.area()`, `animal = None`.
//!    Gates: faction has `ARD_PLOW` tech + ≥1 `ard_plow` in storage.
//! 3. **Claim (`jobs::job_claim_system`)** — the assigned farmer (or any
//!    Farmer if open) picks up the posting, gets `JobClaim { kind: Plow }`.
//! 4. **Dispatcher (`htn_plow_dispatch_system`, ParallelB)** — for each
//!    claimant with `AgentGoal::Farm` and idle queue, reads the posting,
//!    picks the next un-plowed tile via row-major scan indexed by
//!    `plowed_tiles`, picks an un-claimed trained Cattle / Horse (or
//!    re-uses the posting's `animal` once stamped), routes the worker to
//!    that tile, dispatches `Task::Plow { plot_entity, animal }`. On
//!    first dispatch (`plowed_tiles == 0`) the dispatcher also stamps
//!    `animal` onto the posting and inserts `AnimalWorkClaim` on the ox.
//! 5. **Executor (`plow_task_system`, Sequential)** — per tile,
//!    accumulates `PLOW_WORK_TICKS_PER_TILE` ticks then credits
//!    `posting.plowed_tiles += 1` via `record_progress_filtered`. When
//!    the final tile completes, the helper stamps the posting as done +
//!    fires `JobCompletedEvent { completed: true }`; the executor stamps
//!    `Plot.plowed_year`, releases the `AnimalWorkClaim`, and removes
//!    the worker's `JobClaim`.
//! 6. **Tilled stamp (`production_system` Planter branch)** — when a
//!    Grain plant is sown on a tile inside a plot with `plowed_year ==
//!    Some(current_year)`, the plant gets the `Tilled` marker.
//! 7. **Yield bonus (`gather_system` Grain branch)** — `Tilled` plants
//!    harvest at `PLOW_YIELD_MULT_NUMER / PLOW_YIELD_MULT_DENOM = 7/5`
//!    (= 1.4×) of the nutrient-tier base.

use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::animals::{
    AnimalUse, AnimalWorkClaim, DomesticAnimal, DomesticSpecies, Tamed,
};
use crate::simulation::faction::FactionMember;
use crate::simulation::goals::AgentGoal;
use crate::simulation::jobs::{
    record_progress_filtered, JobBoard, JobClaim, JobCompletedEvent, JobKind, JobProgress,
};
use crate::simulation::land::Plot;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, Person, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task, UNEMPLOYED_TASK_KIND};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::{Calendar, TICKS_PER_DAY};
use crate::world::terrain::world_to_tile;

// ── tunables ────────────────────────────────────────────────────────────────

/// Plowed-Grain harvest multiplier numerator (1.4× ≈ 7/5 in integer math).
/// Applied to the base `grain_yield_for_nutrients` tier *before* faction
/// activity multipliers (food_mul) so the bonus stacks linearly with fertility.
pub const PLOW_YIELD_MULT_NUMER: u32 = 7;
pub const PLOW_YIELD_MULT_DENOM: u32 = 5;

/// Per-tile work-tick cost when an ox/horse pulls the plow.
/// Each `Task::Plow` dispatch covers exactly one tile; the dispatcher
/// re-fires for the next tile until the posting completes. At 20 Hz,
/// 6 ticks ≈ 0.3s per tile — a 16×16 plot takes ~75 game-seconds.
pub const PLOW_WORK_TICKS_PER_TILE_ANIMAL: u8 = 6;

/// Per-tile work-tick cost for human-drawn plowing (no ox available).
/// 2× the animal-drawn cost — historically peasants without draft animals
/// yoked themselves to the ard and made slower progress. Same yield bonus;
/// the difference is throughput.
pub const PLOW_WORK_TICKS_PER_TILE_HUMAN: u8 = 12;

/// Legacy alias retained for backwards-compat with the original v2.0
/// constant. Equal to the animal-drawn rate.
pub const PLOW_WORK_TICKS_PER_TILE: u8 = PLOW_WORK_TICKS_PER_TILE_ANIMAL;

/// Per-tile work-tick cost for the named `Task::Plow.animal` mode.
#[inline]
pub fn plow_work_ticks(animal: Option<Entity>) -> u8 {
    if animal.is_some() {
        PLOW_WORK_TICKS_PER_TILE_ANIMAL
    } else {
        PLOW_WORK_TICKS_PER_TILE_HUMAN
    }
}

/// Threshold on `DomesticAnimal.training` above which an animal is eligible
/// for the `AnimalUse::Plow` claim. Cattle / Horse reach this in ~80 days
/// (~3 seasons) of housing once `animal_training_progress_system` is running.
pub const TRAINING_THRESHOLD_DRAFT: u8 = 80;

/// Per-pass training increment in `animal_training_progress_system`.
pub const TRAINING_INCREMENT_PER_PASS: u8 = 1;

/// TTL on `AnimalWorkClaim` for plowing. The executor explicit-releases on
/// the final tile; this is a backstop for stranded claims (worker died,
/// posting evicted) so the ox isn't permanently reserved.
pub const PLOW_CLAIM_TTL_TICKS: u32 = (TICKS_PER_DAY as u32).saturating_mul(2);

// ── components ──────────────────────────────────────────────────────────────

/// Marker on a `Plant` entity born inside a plot that was plowed *this same
/// calendar year*. Stamped at planting (`production_system`); read at harvest
/// (`gather_system`) to apply `PLOW_YIELD_MULT_*`. Persists for the plant's
/// life — re-plowing next year doesn't retroactively tile last year's
/// stragglers; only the next planting picks up the marker.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Tilled;

// ── helpers ────────────────────────────────────────────────────────────────

/// Public helper used by every plow executor exit so the early-release path
/// for `AnimalWorkClaim` is uniform (the only other code that touches the
/// component is `animal_work_claim_expiry_system`'s TTL sweep).
pub fn release_animal_work_claim(commands: &mut Commands, animal: Entity) {
    commands.entity(animal).remove::<AnimalWorkClaim>();
}

/// Multiply a base grain yield by the plow bonus. Pure helper; centralised so
/// the constant is defined once.
#[inline]
pub fn apply_plow_yield_bonus(base: u32) -> u32 {
    (base.saturating_mul(PLOW_YIELD_MULT_NUMER)) / PLOW_YIELD_MULT_DENOM
}

/// Row-major scan: given an inclusive AABB and a tile index `n` (0-based),
/// returns the n-th tile in scan order. Used by the plow dispatcher to walk
/// the plot tile-by-tile keyed on `posting.plowed_tiles`.
#[inline]
pub fn tile_at_index(min: (i32, i32), max: (i32, i32), n: u32) -> Option<(i32, i32)> {
    if min.0 > max.0 || min.1 > max.1 {
        return None;
    }
    let w = (max.0 - min.0 + 1) as u32;
    let h = (max.1 - min.1 + 1) as u32;
    if n >= w.saturating_mul(h) {
        return None;
    }
    let dy = (n / w) as i32;
    let dx = (n % w) as i32;
    Some((min.0 + dx, min.1 + dy))
}

// ── animal training progress ───────────────────────────────────────────────

/// Sequential cadence system: every `TICKS_PER_DAY/4` ticks, every housed
/// `DomesticAnimal` (Tamed + `preferred_home.is_some()`) gains
/// `TRAINING_INCREMENT_PER_PASS` toward the `TRAINING_THRESHOLD_DRAFT` gate.
/// Cattle / Horse cross 80 in ~80 days (≈ 3 seasons); Pig / Dog / Cat tick
/// the same field but the plow dispatcher's species filter excludes them.
pub fn animal_training_progress_system(
    clock: Res<SimClock>,
    mut q: Query<&mut DomesticAnimal, With<Tamed>>,
) {
    let cadence = (TICKS_PER_DAY as u64 / 4).max(60);
    if clock.tick % cadence != 0 {
        return;
    }
    for mut da in q.iter_mut() {
        if da.preferred_home.is_none() {
            continue;
        }
        if da.training >= 255 {
            continue;
        }
        da.training = da.training.saturating_add(TRAINING_INCREMENT_PER_PASS);
    }
}

// ── plow task executor ─────────────────────────────────────────────────────

/// Sequential executor for `Task::Plow { plot_entity, animal }`. Per-tile
/// work model: each Task::Plow dispatch covers exactly one tile. On tile
/// completion, credits `posting.plowed_tiles += 1` via
/// `record_progress_filtered` (which auto-emits `JobCompletedEvent` +
/// despawns the posting when `plowed_tiles >= target_tiles`). If the
/// posting just completed, the executor also stamps
/// `Plot.plowed_year = Some(calendar.year)`, releases the
/// `AnimalWorkClaim`, removes the worker's `JobClaim`, and grants XP.
///
/// Defence in depth: vanished plot or typed-channel mismatch cancels the
/// chain and releases the claim so the ox isn't stranded until TTL.
pub fn plow_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    mut board: ResMut<JobBoard>,
    mut completed_events: EventWriter<JobCompletedEvent>,
    mut plot_q: Query<&mut Plot>,
    mut workers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut Skills,
            &BucketSlot,
            &LodLevel,
            &JobClaim,
        ),
        With<Person>,
    >,
) {
    for (worker, mut ai, mut aq, mut skills, slot, lod, claim) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Plow as u16 {
            continue;
        }
        let Some((plot_entity, animal)) = aq.current.as_plow() else {
            // Defence in depth: typed channel disagrees with task discriminant.
            aq.cancel_chain(&mut ai);
            continue;
        };
        if ai.state != AiState::Working {
            continue;
        }
        if !matches!(claim.kind, JobKind::Plow) {
            // Worker has lost its plow claim somehow; drop the chain.
            if let Some(a) = animal {
                release_animal_work_claim(&mut commands, a);
            }
            aq.cancel_chain(&mut ai);
            continue;
        }
        // Per-tile work threshold (animal: 6, human: 12).
        let needed = plow_work_ticks(animal) as u32;
        if (ai.work_progress as u32) < needed {
            continue;
        }
        // One tile done — credit posting progress and check completion.
        let mut completed = false;
        if let Some(posting) = board.get_mut(claim.job_id) {
            if let JobProgress::Plow {
                plowed_tiles,
                target_tiles,
                ..
            } = &mut posting.progress
            {
                *plowed_tiles = plowed_tiles.saturating_add(1);
                if *plowed_tiles >= *target_tiles {
                    completed = true;
                }
            }
        }
        // Always grant per-tile XP — even mid-job tiles earn the farmer skill.
        skills.gain_xp(SkillKind::Farming, 2);

        if completed {
            // Stamp plot.plowed_year, fire JobCompletedEvent, despawn the
            // posting, release the animal claim (if any), drop the worker's
            // JobClaim, finish the chain.
            if let Ok(mut plot) = plot_q.get_mut(plot_entity) {
                plot.plowed_year = Some(calendar.year as u16);
            }
            record_progress_filtered(
                &mut commands,
                &mut board,
                &mut completed_events,
                claim,
                JobKind::Plow,
                None,
                0,
            );
            if let Some(a) = animal {
                release_animal_work_claim(&mut commands, a);
            }
            commands.entity(worker).remove::<JobClaim>();
            skills.gain_xp(SkillKind::Farming, 6); // completion bonus
            aq.finish_task(&mut ai);
        } else {
            // Mid-job: drop back to Idle so the next dispatcher pass picks
            // the next tile. Keep the JobClaim, keep the AnimalWorkClaim
            // (if any).
            aq.finish_task(&mut ai);
        }
    }
}

// ── plow dispatcher ────────────────────────────────────────────────────────

/// ParallelB dispatcher. For each `JobClaim::Plow` holder on goal
/// `AgentGoal::Farm` (Plow piggybacks on Farm), reads the posting, picks the
/// next un-plowed tile via row-major scan keyed on `plowed_tiles`, picks an
/// un-claimed trained Cattle / Horse (or re-uses the posting's stamped
/// animal), routes the worker, and dispatches `Task::Plow`.
///
/// On first dispatch (`plowed_tiles == 0` AND `posting.animal.is_none()`)
/// the dispatcher selects an animal, stamps it onto the posting, and inserts
/// `AnimalWorkClaim` on it. Subsequent dispatches reuse the same animal.
pub fn htn_plow_dispatch_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    mut board: ResMut<JobBoard>,
    claim_q: Query<&AnimalWorkClaim>,
    animals_q: Query<(Entity, &DomesticAnimal, &Tamed)>,
    mut workers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
            &BucketSlot,
            &JobClaim,
        ),
        (With<Person>, Without<Drafted>),
    >,
    spatial_index: Res<crate::world::spatial::SpatialIndex>,
    stand_reservations: Res<crate::simulation::stand_reservation::StandTileReservations>,
) {
    let now = clock.tick;
    let now_u32 = now as u32;
    for (worker, mut ai, mut aq, goal, fm, tr, lod, slot, claim) in workers.iter_mut() {
        let actor = worker;
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if !matches!(claim.kind, JobKind::Plow) {
            continue;
        }
        if !matches!(*goal, AgentGoal::Farm) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        let Some(posting) = board.get_mut(claim.job_id) else {
            continue;
        };
        let (plot_id, area, plowed_tiles, target_tiles, posted_animal) = match posting.progress {
            JobProgress::Plow {
                plot_id,
                area,
                plowed_tiles,
                target_tiles,
                animal,
                ..
            } => (plot_id, area, plowed_tiles, target_tiles, animal),
            _ => continue,
        };
        if plowed_tiles >= target_tiles {
            continue;
        }
        // Pick the next tile in scan order.
        let Some(tile) = tile_at_index(area.min, area.max, plowed_tiles) else {
            continue;
        };
        // Resolve plot entity for the Task::Plow variant.
        // PlotIndex lookup via the posting's plot_id.
        // We need the entity. Pull it lazily through a SystemParam — but
        // since we already have board as ResMut, we can fetch PlotIndex
        // via a separate query. Simpler: snapshot up-front. We do an
        // inline lookup here via Commands-free path — see below.
        // Reuse the worker's tile.
        let worker_tile = world_to_tile(tr.translation.truncate());
        let cur_chunk = ChunkCoord(
            worker_tile.0.div_euclid(CHUNK_SIZE as i32),
            worker_tile.1.div_euclid(CHUNK_SIZE as i32),
        );
        // Pick / re-use animal. Three branches:
        //  - posted_animal == Some — validated and reused for the rest of the job
        //  - posted_animal == None, ox available — commit + stamp + claim
        //  - posted_animal == None, no ox — fall back to human-drawn (animal = None)
        //
        // The third branch leaves `posting.animal` as None so the *next*
        // dispatch may upgrade if an ox becomes free mid-job. Slower (12 ticks
        // per tile vs 6) but never blocks.
        let animal: Option<Entity> = match posted_animal {
            Some(a) => {
                // Validate the animal still exists with valid Tamed + DomesticAnimal.
                if animals_q.get(a).is_err() {
                    // Animal died / despawned mid-job — un-stamp from the posting
                    // and fall through to a fresh search below.
                    if let Some(p2) = board.get_mut(claim.job_id) {
                        if let JobProgress::Plow {
                            animal: ref mut posting_animal,
                            ..
                        } = p2.progress
                        {
                            *posting_animal = None;
                        }
                    }
                    pick_idle_draft_animal(fm.faction_id, &animals_q, &claim_q)
                } else {
                    Some(a)
                }
            }
            None => pick_idle_draft_animal(fm.faction_id, &animals_q, &claim_q),
        };
        // Route worker to the tile.
        let routed = assign_task_with_routing(
            &mut ai,
            worker_tile,
            cur_chunk,
            tile,
            TaskKind::Plow,
            None,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
            &spatial_index,
            &stand_reservations,
            actor,
            now,);
        if !routed {
            continue;
        }
        // Resolve plot entity for the Task::Plow variant via PlotIndex.
        let Some(&plot_entity) = plot_index.by_id.get(&plot_id) else {
            continue;
        };
        let _ = aq.dispatch(Task::Plow {
            plot_entity,
            animal,
        });
        // Stamp animal on the posting + insert the work claim ONLY when this
        // dispatch newly committed to an ox (i.e. we picked one and the
        // posting wasn't already tracking it). Human-drawn dispatches
        // (animal == None) leave `posting.animal` as None so the next
        // dispatcher pass can upgrade if an ox becomes free.
        if let Some(a) = animal {
            if posted_animal != Some(a) {
                if let Some(p2) = board.get_mut(claim.job_id) {
                    if let JobProgress::Plow {
                        animal: posting_animal,
                        ..
                    } = &mut p2.progress
                    {
                        *posting_animal = Some(a);
                    }
                }
                commands.entity(a).insert(AnimalWorkClaim {
                    worker,
                    use_kind: AnimalUse::Plow,
                    expires_tick: now_u32.saturating_add(PLOW_CLAIM_TTL_TICKS),
                });
            }
        }
    }
}

/// Find one trained, un-claimed Cattle / Horse owned by `faction_id` with
/// no live `AnimalWorkClaim`. Returns `None` when no eligible draft animal
/// exists — caller falls back to human-drawn plowing.
fn pick_idle_draft_animal(
    faction_id: u32,
    animals_q: &Query<(Entity, &DomesticAnimal, &Tamed)>,
    claim_q: &Query<&AnimalWorkClaim>,
) -> Option<Entity> {
    for (e, da, tamed) in animals_q.iter() {
        if tamed.owner_faction != faction_id {
            continue;
        }
        if da.training < TRAINING_THRESHOLD_DRAFT {
            continue;
        }
        if !matches!(da.species, DomesticSpecies::Cattle | DomesticSpecies::Horse) {
            continue;
        }
        if claim_q.get(e).is_ok() {
            continue;
        }
        return Some(e);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plow_yield_bonus_applies_seven_fifths_per_tier() {
        assert_eq!(apply_plow_yield_bonus(5), 7);
        assert_eq!(apply_plow_yield_bonus(4), 5);
        assert_eq!(apply_plow_yield_bonus(3), 4);
        assert_eq!(apply_plow_yield_bonus(1), 1);
        assert_eq!(apply_plow_yield_bonus(0), 0);
    }

    #[test]
    fn plow_yield_bonus_is_monotonic_increasing() {
        let bases = [1u32, 3, 4, 5];
        let bonused: Vec<u32> = bases.iter().map(|b| apply_plow_yield_bonus(*b)).collect();
        for (b, p) in bases.iter().zip(bonused.iter()) {
            assert!(
                *p >= *b,
                "plow bonus must not reduce yield (base {b} → {p})"
            );
        }
    }

    #[test]
    fn training_threshold_is_reachable() {
        assert!(TRAINING_THRESHOLD_DRAFT < 255);
        assert!(TRAINING_INCREMENT_PER_PASS > 0);
        let passes_needed = TRAINING_THRESHOLD_DRAFT.div_ceil(TRAINING_INCREMENT_PER_PASS);
        assert!(passes_needed <= 120);
    }

    #[test]
    fn tile_at_index_walks_row_major() {
        // 3×2 rect: (0,0) (1,0) (2,0) (0,1) (1,1) (2,1)
        assert_eq!(tile_at_index((0, 0), (2, 1), 0), Some((0, 0)));
        assert_eq!(tile_at_index((0, 0), (2, 1), 1), Some((1, 0)));
        assert_eq!(tile_at_index((0, 0), (2, 1), 2), Some((2, 0)));
        assert_eq!(tile_at_index((0, 0), (2, 1), 3), Some((0, 1)));
        assert_eq!(tile_at_index((0, 0), (2, 1), 5), Some((2, 1)));
        assert_eq!(tile_at_index((0, 0), (2, 1), 6), None);
        // Offset min: same shape, shifted.
        assert_eq!(tile_at_index((10, 20), (12, 21), 4), Some((11, 21)));
    }

    #[test]
    fn plow_total_work_is_bounded_for_default_plot() {
        let area: u32 = 16 * 16;
        let total = area.saturating_mul(PLOW_WORK_TICKS_PER_TILE as u32);
        assert!(total >= 200);
        assert!(total <= crate::world::seasons::TICKS_PER_DAY);
    }
}
