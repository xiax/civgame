//! Goal/dispatcher contract layer.
//!
//! A goal *scorer* (`goal_scorers.rs`) may only select a goal whose HTN
//! *dispatcher* (`htn.rs`) can actually decompose into a task. When a scorer
//! emits a goal with no executable target, the dispatcher silently `continue`s,
//! the agent sits `Idle`, and — because idle agents bypass the 200-tick
//! goal-update cadence — the scorer re-selects the same indecomposable goal
//! every tick. An infinite idle loop.
//!
//! This module is the single home for:
//! - **Centralized scan-radius constants** so a scorer-side gate and its
//!   dispatcher-side scan cannot drift.
//! - **`BlockedReason`** — structured diagnostics for the dev-build logging at
//!   each dispatcher no-task `continue`.
//! - **`record_no_task_backstop`** — the structural backstop: when a dispatcher
//!   cannot produce a task it records a throttled synthetic `MethodHistory`
//!   failure, so `chronic_failure_release_system` eventually cooldowns the goal
//!   and frees the agent even if a scorer gate was missed or briefly stale.
//!
//! See `src/simulation/CLAUDE.md` for the survival-maintenance invariant this
//! generalizes.

use bevy::prelude::*;

use crate::simulation::goals::AgentGoal;
use crate::simulation::htn::{record_target_failure, MethodHistory};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, PersonAI, UNEMPLOYED_TASK_KIND};
use crate::simulation::schedule::SimClock;
use crate::simulation::typed_task::ActionQueue;

// ─── Centralized scan-radius constants ─────────────────────────────────────

/// Radius `htn_tame_animal_dispatch_system` scans `SpatialIndex` for an untamed
/// candidate. The `TameAnimalScorer` gate must use the same radius.
pub const TAME_SEARCH_RADIUS: i32 = 15;

/// Radius `htn_socialize_dispatch_system` scans for a dedicated-Socialize
/// partner. Distinct from `social_contact::SOCIAL_RADIUS` (3), which governs
/// *ambient* work-pairing — leave that alone.
pub const SOCIAL_PARTNER_RADIUS: i32 = 12;

/// Radius `htn_play_dispatch_system` scans for a play partner.
pub const PLAY_PARTNER_RADIUS: i32 = 12;

/// Radius `htn_play_dispatch_system` scans for a play item / entertainment good.
pub const PLAY_ITEM_RADIUS: i32 = 8;

// Re-export the canonical definitions from their owning subsystems so callers
// have one import point and the scorer/dispatcher cannot disagree.
pub use crate::simulation::drink::{DRINK_HOME_SCAN_RADIUS, DRINK_TILE_SCAN_RADIUS};
pub use crate::simulation::medicine::{HEAL_SCAN_RADIUS, SEEK_CARE_AT_SITE_RADIUS};

// ─── Structured blocked reasons (dev-build diagnostics) ────────────────────

/// Why a dispatcher selected for a goal could not produce a task. Used only by
/// the `#[cfg(debug_assertions)]` logging at each no-task `continue` and as a
/// human-readable label for the backstop — release builds compile the logging
/// out, so this carries no runtime cost in production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    /// `TameAnimal`: no tech-eligible untamed candidate within `TAME_SEARCH_RADIUS`.
    NoLocalTameTarget,
    /// `ProvideCare`: no injured same-faction patient within `HEAL_SCAN_RADIUS`.
    NoCarePatient,
    /// `Socialize`: no reachable dedicated-social partner nearby.
    NoPartner,
    /// `Play`: no nearby partner, toy, entertainment resource, or plantable.
    NoPlayOption,
    /// `Craft`: no live actionable `CraftOrder` path (deliver / work / harvest).
    NoCraftOrderPath,
    /// `Build`: owned blueprint but no executable material path.
    NoBuildMaterialPath,
    /// `Farm`: no executable in-season farm phase work.
    NoFarmPhaseWork,
    /// `Drink`: no routable water / storage source.
    NoDrinkSource,
    /// HTN argmax produced no method, or the method expanded to no tasks.
    NoMethod,
}

impl BlockedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedReason::NoLocalTameTarget => "no local tame target",
            BlockedReason::NoCarePatient => "no local care patient",
            BlockedReason::NoPartner => "no social partner",
            BlockedReason::NoPlayOption => "no play option",
            BlockedReason::NoCraftOrderPath => "no craft order path",
            BlockedReason::NoBuildMaterialPath => "no build material path",
            BlockedReason::NoFarmPhaseWork => "no farm phase work",
            BlockedReason::NoDrinkSource => "no drink source",
            BlockedReason::NoMethod => "no method / empty expansion",
        }
    }
}

// ─── No-task backstop ──────────────────────────────────────────────────────

/// Minimum ticks between two consecutive synthetic backstop failures for one
/// agent. Idle agents re-enter dispatch every tick; without throttling they
/// would burn the (length-6) `MethodHistory` ring in a few ticks and trip
/// `chronic_failure_release` near-instantly. ~20 ticks paces recovery across a
/// few cadence windows, matching the chronic-release design intent.
pub const BACKSTOP_THROTTLE_TICKS: u64 = 20;

/// Structural backstop for the goal/dispatcher contract.
///
/// Call at a dispatcher's no-task `continue` (the path taken when the goal was
/// selected but no executable target exists). Records a throttled synthetic
/// non-success entry on the agent's `MethodHistory` via `record_target_failure`
/// (which stamps `MethodId::UNKNOWN` when `active_method` is `None`). After
/// `CHRONIC_FAIL_THRESHOLD` such entries accumulate within the history TTL,
/// `chronic_failure_release_system` stamps a `GoalCooldown` and forces a goal
/// re-evaluation — so even a goal whose scorer gate was missed or briefly stale
/// recovers instead of looping forever.
///
/// `MethodId::UNKNOWN` entries are filtered out of `recently_failed_count` (the
/// per-method failure bias) so they do not poison method scoring, but they DO
/// count toward `chronic_failure_release_system`'s plain non-success tally —
/// which is exactly the channel this backstop uses.
///
/// Returns `true` when a failure was actually recorded (not throttled).
pub fn record_no_task_backstop(history: &mut MethodHistory, ai: &mut PersonAI, now: u64) -> bool {
    let recently_recorded = history
        .entries
        .iter()
        .flatten()
        .any(|(_, _, tick)| now.saturating_sub(*tick) < BACKSTOP_THROTTLE_TICKS);
    if recently_recorded {
        return false;
    }
    record_target_failure(history, ai, now);
    true
}

/// Dev-build structured log for a dispatcher no-task `continue`, plus the
/// throttled backstop. Release builds compile the log call out; the backstop
/// always runs. Call site:
/// `goal_contract::blocked(&mut history, &mut ai, now, goal, reason)`.
#[inline]
pub fn blocked(
    history: &mut MethodHistory,
    ai: &mut PersonAI,
    now: u64,
    goal: crate::simulation::goals::AgentGoal,
    reason: BlockedReason,
) {
    #[cfg(debug_assertions)]
    {
        bevy::log::debug!(
            target: "goal_contract",
            "agent goal {:?} blocked: {}",
            goal,
            reason.as_str(),
        );
    }
    #[cfg(not(debug_assertions))]
    let _ = (goal, reason);
    record_no_task_backstop(history, ai, now);
}

/// Goals whose dispatch is spread across **multiple** dispatcher systems
/// (`Craft` → deliver / work / harvest; `Build` Path B; `Farm` →
/// prepare / plant / harvest). A per-dispatcher backstop can't cleanly tell
/// "this dispatcher couldn't serve me" from "a sibling dispatcher will" — so
/// these are covered by the generic post-ParallelB `goal_contract_backstop_system`
/// instead. Single-dispatcher goals (`TameAnimal`, `ProvideCare`, `Socialize`,
/// `Play`) carry their backstop inline at the dispatcher's no-task `continue`.
#[inline]
pub fn is_multi_dispatcher_contract_goal(goal: AgentGoal) -> bool {
    matches!(goal, AgentGoal::Craft | AgentGoal::Build | AgentGoal::Farm)
}

/// Generic no-task backstop for multi-dispatcher contract goals.
///
/// Runs at the head of `SimulationSet::Sequential` — i.e. after every ParallelB
/// HTN dispatcher has had its chance. Any non-Dormant agent still `Idle` with an
/// empty `ActionQueue` while holding a multi-dispatcher contract goal had its
/// goal selected but produced no task; record a throttled synthetic failure so
/// `chronic_failure_release_system` eventually cooldowns the goal and frees the
/// agent. The 20-tick throttle means a normal one-tick gap between tasks never
/// accumulates toward the chronic-failure threshold — only a genuinely stuck
/// agent (idle for tens of ticks on the same goal) does.
pub fn goal_contract_backstop_system(
    clock: Res<SimClock>,
    mut query: Query<
        (
            &AgentGoal,
            &mut PersonAI,
            &ActionQueue,
            &mut MethodHistory,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    let now = clock.tick;
    for (goal, mut ai, aq, mut history, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !is_multi_dispatcher_contract_goal(*goal) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        record_no_task_backstop(&mut history, &mut ai, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::htn::{MethodHistory, MethodOutcome, METHOD_HISTORY_TTL_TICKS};

    fn non_success_count(history: &MethodHistory, now: u64) -> u32 {
        history
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
            .count() as u32
    }

    #[test]
    fn backstop_throttles_within_window() {
        let mut history = MethodHistory::default();
        let mut ai = PersonAI::default();
        // First call at tick 100 records.
        assert!(record_no_task_backstop(&mut history, &mut ai, 100));
        // A call inside the throttle window is suppressed.
        assert!(!record_no_task_backstop(&mut history, &mut ai, 100 + 1));
        assert!(!record_no_task_backstop(
            &mut history,
            &mut ai,
            100 + BACKSTOP_THROTTLE_TICKS - 1
        ));
        // Past the window it records again.
        assert!(record_no_task_backstop(
            &mut history,
            &mut ai,
            100 + BACKSTOP_THROTTLE_TICKS
        ));
    }

    #[test]
    fn backstop_accumulates_to_chronic_failure_threshold() {
        // A stuck agent re-entering dispatch every tick must, with the 20-tick
        // throttle, accumulate >= 3 non-success entries (the
        // `chronic_failure_release_system` threshold) within the history TTL.
        let mut history = MethodHistory::default();
        let mut ai = PersonAI::default();
        let mut tick = 0u64;
        for _ in 0..200 {
            record_no_task_backstop(&mut history, &mut ai, tick);
            tick += 1;
        }
        // 200 ticks of continuous idle, throttled at 20 → ~10 records, all
        // within TTL=600. Threshold for chronic release is 3.
        assert!(non_success_count(&history, tick) >= 3);
    }
}
