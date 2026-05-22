//! `Energy` — a physiological exertion / fatigue resource.
//!
//! Energy is deliberately a **separate component**, not a `Needs` field.
//! `Needs::worst()` / `avg_distress()` drive mood; energy is physiology, not
//! morale, so it must NOT feed those. Keeping it standalone is the whole
//! reason for the separate component.
//!
//! Energy drains while an agent moves, labors, and fights, and recovers
//! while it sleeps (fast) or idles (slow). Low energy slows movement and
//! work via `energy_factor()`, and an *exhausted* agent stops picking up
//! noncritical heavy-labor goals (a gate in `GoalScoringContext`) until it
//! has recovered. The 0–255 float range mirrors the `Needs` convention.

use bevy::prelude::*;

use super::lod::LodLevel;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};

/// Energy ceiling, mirroring the `Needs` 0–255 float convention.
pub const ENERGY_MAX: f32 = 255.0;

/// Below this, an agent flips to the *exhausted* state — its noncritical
/// heavy-labor goals are gated off and it slumps toward the work/movement
/// floor. Hysteresis: it stays exhausted until energy climbs past
/// `ENERGY_RECOVERED`.
pub const ENERGY_EXHAUSTED: f32 = 40.0;
/// Below this an agent is *tired* — combat cooldowns lengthen and the
/// `energy_factor()` ramp begins biting.
pub const ENERGY_TIRED: f32 = 90.0;
/// An exhausted agent must climb back past this before the exhausted flag
/// clears (hysteresis — prevents flicker around `ENERGY_EXHAUSTED`).
pub const ENERGY_RECOVERED: f32 = 140.0;

/// Floor of `energy_factor()` — a fully-drained agent still works/moves at
/// 45% effectiveness (mirrors `medicine::sickness_work_factor`'s shape).
pub const ENERGY_FACTOR_FLOOR: f32 = 0.45;

// ── Rates (per real second; tick systems scale by `dt` like `Needs`). ──

/// Energy drained per second of active labor (`AiState::Working`).
pub const ENERGY_LABOR_DRAIN: f32 = 1.5;
/// Energy drained per attack swing in `combat_system`.
pub const ENERGY_ATTACK_DRAIN: f32 = 6.0;
/// Base energy drained per tile of on-foot travel.
pub const ENERGY_MOVE_DRAIN_PER_TILE: f32 = 0.35;
/// Mounted travel multiplier — riding is far less tiring than walking.
pub const ENERGY_MOVE_MOUNTED_SCALE: f32 = 0.15;
/// Energy recovered per second while awake and idle.
pub const ENERGY_IDLE_RECOVER: f32 = 0.8;
/// Energy recovered per second while `AiState::Sleeping` (doubled on a bed,
/// mirroring the bed bonus on `SLEEP_RECOVER_RATE`).
pub const ENERGY_SLEEP_RECOVER: f32 = 8.0;

/// Per-agent exertion / fatigue pool. `max` exists for a future
/// Constitution-scaled capacity; v1 leaves it flat at `ENERGY_MAX`.
#[derive(Component, Clone, Copy, Debug)]
pub struct Energy {
    pub current: f32,
    pub max: f32,
    /// Stateful hysteresis flag. Set when `current` drops below
    /// `ENERGY_EXHAUSTED`; cleared only once `current` climbs past
    /// `ENERGY_RECOVERED`.
    pub exhausted: bool,
}

impl Default for Energy {
    fn default() -> Self {
        Self {
            current: ENERGY_MAX,
            max: ENERGY_MAX,
            exhausted: false,
        }
    }
}

impl Energy {
    /// Construct with an explicit starting `current` (clamped). Used by the
    /// test fixture's `.energy(..)` override.
    pub fn new(current: f32) -> Self {
        let mut e = Self {
            current: current.clamp(0.0, ENERGY_MAX),
            max: ENERGY_MAX,
            exhausted: false,
        };
        e.refresh_flag();
        e
    }

    /// Re-evaluate the stateful `exhausted` flag after `current` changes.
    pub fn refresh_flag(&mut self) {
        if self.exhausted {
            if self.current >= ENERGY_RECOVERED {
                self.exhausted = false;
            }
        } else if self.current < ENERGY_EXHAUSTED {
            self.exhausted = true;
        }
    }

    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    pub fn is_tired(&self) -> bool {
        self.current < ENERGY_TIRED
    }

    /// Drain by `amount` (clamped at 0), refreshing the hysteresis flag.
    pub fn drain(&mut self, amount: f32) {
        self.current = (self.current - amount.max(0.0)).max(0.0);
        self.refresh_flag();
    }

    /// Recover by `amount` (clamped at `max`), refreshing the flag.
    pub fn recover(&mut self, amount: f32) {
        self.current = (self.current + amount.max(0.0)).min(self.max);
        self.refresh_flag();
    }

    /// Work / movement effectiveness multiplier. 1.0 while fresh
    /// (`current >= ENERGY_TIRED`), ramping linearly down to
    /// `ENERGY_FACTOR_FLOOR` as energy approaches 0 — mirrors the
    /// `medicine::sickness_work_factor` shape.
    pub fn energy_factor(&self) -> f32 {
        if self.current >= ENERGY_TIRED {
            return 1.0;
        }
        let t = (self.current / ENERGY_TIRED).clamp(0.0, 1.0);
        ENERGY_FACTOR_FLOOR + (1.0 - ENERGY_FACTOR_FLOOR) * t
    }
}

/// Labor drain + idle recovery, mirroring `tick_needs_system`'s structure.
///
/// Movement drain (per tile) is owned by `movement::movement_system`,
/// combat drain (per swing) by `combat::combat_system`, and sleep recovery
/// by `sleep::sleep_task_system` — this system covers the remaining two
/// cases keyed on `AiState`:
/// - `Working` → drain at `ENERGY_LABOR_DRAIN` (work progress advances).
/// - `Idle` → recover at `ENERGY_IDLE_RECOVER`.
/// `Sleeping` / `Attacking` / `Seeking` / `Routing` are skipped — their
/// energy is owned by the systems above.
pub fn energy_tick_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    mut query: Query<(&BucketSlot, &LodLevel, &PersonAI, &mut Energy)>,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    query.par_iter_mut().for_each(|(slot, lod, ai, mut energy)| {
        if *lod == LodLevel::Dormant {
            return;
        }
        if !clock.is_active(slot.0) {
            return;
        }
        match ai.state {
            AiState::Working => energy.drain(ENERGY_LABOR_DRAIN * dt),
            AiState::Idle => energy.recover(ENERGY_IDLE_RECOVER * dt),
            // Sleeping → sleep_task_system; Attacking → combat_system;
            // Seeking/Routing → movement_system per-tile drain.
            AiState::Sleeping | AiState::Attacking | AiState::Seeking | AiState::Routing => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_energy_is_full_and_not_exhausted() {
        let e = Energy::default();
        assert_eq!(e.current, ENERGY_MAX);
        assert!(!e.is_exhausted());
        assert_eq!(e.energy_factor(), 1.0);
    }

    #[test]
    fn exhaustion_has_hysteresis() {
        let mut e = Energy::new(ENERGY_EXHAUSTED + 1.0);
        assert!(!e.is_exhausted());
        e.drain(5.0);
        assert!(e.is_exhausted());
        // Climbing past EXHAUSTED is not enough — must reach RECOVERED.
        e.recover(ENERGY_RECOVERED - ENERGY_EXHAUSTED - 5.0);
        assert!(e.is_exhausted());
        e.recover(10.0);
        assert!(!e.is_exhausted());
    }

    #[test]
    fn energy_factor_floor() {
        let e = Energy::new(0.0);
        assert!((e.energy_factor() - ENERGY_FACTOR_FLOOR).abs() < 1e-4);
        let fresh = Energy::new(ENERGY_TIRED);
        assert_eq!(fresh.energy_factor(), 1.0);
    }
}
