//! Sleep executor — owns the full `Task::Sleep` lifecycle.
//!
//! `Task::Sleep` used to be the only typed task with no dedicated executor:
//! `htn_dispatch_system` planned/routed it and flipped `AiState::Working ->
//! Sleeping` on arrival, while `needs::tick_needs_system` did the recovery and
//! the *sole* retirement — but only while `ai.state == AiState::Sleeping`. Any
//! system that reset `ai.state -> Idle` while leaving `aq.current == Sleep`
//! (notably `combat_system` retaliation whose deferred cancel is gated behind
//! `target_combat.0.is_none()`) stranded the task with no retirement path;
//! `goal_dispatch_system`'s Sleep preserve-arm kept `current == Sleep` alive,
//! so the next `htn_dispatch_system` tick re-dispatched Sleep while `current`
//! was still Sleep and the queue empty -> `ActionQueue::dispatch` desync panic.
//!
//! This executor removes that fragility class. It is keyed on the typed task
//! (`aq.current_task_kind() == TaskKind::Sleep`), not on `ai.state`, so it runs
//! every tick a Sleep task is live and owns:
//!
//! - **Arrival -> Sleeping** (was htn `htn_dispatch_system` guard-4): movement
//!   sets `Working` when a routed Sleep reaches its destination; flip to
//!   `Sleeping`.
//! - **Recovery + retirement** (was `needs::tick_needs_system`): drain
//!   `needs.sleep` (2x on a bed), raise `needs.willpower`, and `finish_task`
//!   when rested. Reachable because it keys on the task, not on the state.
//! - **Orphan self-heal** (mirrors `drink_task_system`'s `as_drink()` else
//!   branch): if `current == Sleep` but the agent is neither asleep nor
//!   walking toward sleep (`Idle`/`Attacking` after an external preempt),
//!   `cancel_chain` so the dispatcher cleanly re-plans+re-routes. This is the
//!   structural reason the desync panic is now impossible: the orphan is
//!   cleared *before* the next `htn_dispatch_system` tick.

use bevy::prelude::*;

use crate::simulation::construction::BedMap;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::{
    Needs, SLEEP_RECOVER_RATE, SLEEP_WAKE_THRESHOLD, WILLPOWER_SLEEP_RECOVER,
};
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::tasks::TaskKind;
use crate::simulation::typed_task::ActionQueue;
use crate::world::terrain::TILE_SIZE;

/// Sequential executor for `Task::Sleep`. Registered after `movement_system`
/// (arrival must be settled) and `combat_retaliation_cleanup_system` (so a
/// victim under attack isn't re-asserted to `Sleeping`), and before
/// `cosleep_observation_system` (the wake becomes visible to household
/// formation / conception on the same tick `tick_needs` used to do it).
pub fn sleep_task_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    bed_map: Res<BedMap>,
    mut query: Query<(
        &mut PersonAI,
        &mut ActionQueue,
        &mut Needs,
        &Transform,
        &BucketSlot,
        &LodLevel,
        Option<&mut crate::simulation::energy::Energy>,
    )>,
) {
    // Bucket compensation only — game speed scales FixedUpdate firing rate.
    let dt = time.delta_secs() * clock.scale_factor();

    query
        .par_iter_mut()
        .for_each(|(mut ai, mut aq, mut needs, transform, slot, lod, mut energy_opt)| {
            // Dormant agents' Sleep tasks are frozen (parity: both the old
            // `tick_needs_system` and `htn_dispatch_system` skipped Dormant).
            if *lod == LodLevel::Dormant {
                return;
            }
            // Source of truth: only act when the typed task is Sleep.
            if aq.current_task_kind() != TaskKind::Sleep as u16 {
                return;
            }
            // Defence in depth: kind says Sleep but the variant disagrees
            // (mirrors `drink_task_system`'s `as_drink()` recovery). Drop the
            // chain so the next dispatch re-plans cleanly.
            if aq.current.as_sleep().is_none() {
                aq.cancel_chain(&mut ai);
                return;
            }

            // State machine — NOT bucket-gated, so an inactive bucket can
            // never permanently strand the task (that was the fragility).
            match ai.state {
                // Walking toward home/bed — let movement carry us (was
                // `htn_dispatch_system` guard-5).
                AiState::Seeking | AiState::Routing => return,
                // Movement reports arrival at the routed sleep destination
                // (it sets Working for any non-Idle current task). Begin
                // sleeping; recovery starts next tick (parity with the old
                // guard-4 -> tick_needs 1-tick handoff).
                AiState::Working => {
                    aq.begin_sleeping(&mut ai);
                    return;
                }
                // Orphan: `current == Sleep` survived but the agent is neither
                // asleep nor walking. The only producers (in-place dispatch
                // sets Sleeping, routed dispatch sets Seeking/Routing) never
                // leave Idle/Attacking with current==Sleep, so this is
                // definitionally an external-preempt orphan. Cancel so the
                // dispatcher re-plans+re-routes — and so the next
                // `htn_dispatch_system` never sees current==Sleep+state==Idle.
                AiState::Idle | AiState::Attacking => {
                    aq.cancel_chain(&mut ai);
                    return;
                }
                AiState::Sleeping => {}
            }

            // Recovery + retirement (state == Sleeping). Rate math is
            // bucket-gated for parity with the old `tick_needs_system`.
            if !clock.is_active(slot.0) {
                return;
            }
            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            // Double recovery when resting on a bed.
            let on_bed = bed_map.0.contains_key(&(cur_tx, cur_ty));
            let mult = if on_bed { 2.0 } else { 1.0 };
            needs.sleep = (needs.sleep - SLEEP_RECOVER_RATE * mult * dt).clamp(0.0, 255.0);
            needs.willpower =
                (needs.willpower + WILLPOWER_SLEEP_RECOVER * mult * dt).clamp(0.0, 255.0);
            // Sleep is the primary energy recovery channel (a bed doubles
            // the rate, same `mult` as sleep/willpower).
            if let Some(energy) = energy_opt.as_deref_mut() {
                energy.recover(crate::simulation::energy::ENERGY_SLEEP_RECOVER * mult * dt);
            }
            if needs.sleep < SLEEP_WAKE_THRESHOLD {
                // Rested — retire the typed Sleep task. `goal_update_system`
                // flips the Sleep goal off on its next cadence; until then
                // this is the canonical wake. Keyed on the task (not on
                // `ai.state`), so it is always reachable.
                aq.finish_task(&mut ai);
            }
        });
}
