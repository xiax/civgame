//! Game-speed and pause control, plus simulation-tick timing diagnostics.
//!
//! `GameSpeed` is the single source of truth for simulation speed and pause.
//! A small `PreUpdate` sync system mirrors it onto `Time<Virtual>` —
//! `pause`/`unpause` + `set_relative_speed` — so every system that ticks
//! once per FixedUpdate (needs, cooldowns, calendar, plant lifecycle, daily
//! Economy systems, etc.) speeds up naturally at higher presets.
//!
//! `SimTimingDiagnostics` tracks ticks/frame and per-tick CPU cost. Surfaced
//! in `ui/debug_panel.rs`; reddens when the rolling average exceeds the
//! current preset's CPU budget.

use bevy::input::ButtonInput;
use bevy::prelude::*;
use bevy_egui::EguiContexts;
use std::collections::VecDeque;
use std::time::Instant;

use crate::simulation::schedule::SimClock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedPreset {
    Paused,
    Normal,
    Fast,
    VeryFast,
}

impl SpeedPreset {
    pub const fn multiplier(self) -> f32 {
        match self {
            Self::Paused => 0.0,
            Self::Normal => 1.0,
            Self::Fast => 2.0,
            Self::VeryFast => 5.0,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Paused => "⏸",
            Self::Normal => "1×",
            Self::Fast => "2×",
            Self::VeryFast => "5×",
        }
    }

    /// Rough per-tick CPU budget at this speed, given a 20 Hz fixed update
    /// and a 50 ms `Time<Virtual>::set_max_delta` cap. The debug panel
    /// reddens when the rolling-average tick exceeds this.
    pub const fn budget_ms_per_tick(self) -> f32 {
        match self {
            Self::Paused => f32::INFINITY,
            Self::Normal => 50.0,
            Self::Fast => 25.0,
            Self::VeryFast => 10.0,
        }
    }

    pub const fn all() -> [Self; 4] {
        [Self::Paused, Self::Normal, Self::Fast, Self::VeryFast]
    }
}

/// Player-facing simulation-speed state. Mirrored onto `Time<Virtual>` by
/// [`sync_game_speed_to_virtual_time`]. `last_unpaused` is preserved so
/// pause → resume restores the prior preset.
#[derive(Resource, Debug, Clone, Copy)]
pub struct GameSpeed {
    pub current: SpeedPreset,
    pub last_unpaused: SpeedPreset,
}

impl Default for GameSpeed {
    fn default() -> Self {
        Self {
            current: SpeedPreset::Normal,
            last_unpaused: SpeedPreset::Normal,
        }
    }
}

impl GameSpeed {
    /// Set to `preset`. Non-paused presets update `last_unpaused` so a
    /// later toggle resumes here.
    pub fn set(&mut self, preset: SpeedPreset) {
        if preset != SpeedPreset::Paused {
            self.last_unpaused = preset;
        }
        self.current = preset;
    }

    /// Toggle pause: pause ↔ `last_unpaused`.
    pub fn toggle_pause(&mut self) {
        if self.current == SpeedPreset::Paused {
            self.current = self.last_unpaused;
        } else {
            self.last_unpaused = self.current;
            self.current = SpeedPreset::Paused;
        }
    }
}

/// PreUpdate: mirror `GameSpeed` onto `Time<Virtual>`. Only writes when
/// `GameSpeed` changed this frame, so it's cheap to schedule every frame.
pub fn sync_game_speed_to_virtual_time(speed: Res<GameSpeed>, mut vtime: ResMut<Time<Virtual>>) {
    if !speed.is_changed() {
        return;
    }
    match speed.current {
        SpeedPreset::Paused => {
            if !vtime.is_paused() {
                vtime.pause();
            }
        }
        other => {
            if vtime.is_paused() {
                vtime.unpause();
            }
            vtime.set_relative_speed(other.multiplier());
        }
    }
}

/// Update: keyboard shortcuts. `Space` toggles pause; `1`/`2`/`3` pick
/// `Normal`/`Fast`/`VeryFast`. Gated on `wants_keyboard_input` so typing
/// into an egui panel doesn't hijack input.
pub fn handle_speed_keybinds_system(
    keys: Res<ButtonInput<KeyCode>>,
    mut speed: ResMut<GameSpeed>,
    mut contexts: EguiContexts,
) {
    if contexts.ctx_mut().wants_keyboard_input() {
        return;
    }
    if keys.just_pressed(KeyCode::Space) {
        speed.toggle_pause();
        return;
    }
    if keys.just_pressed(KeyCode::Digit1) {
        speed.set(SpeedPreset::Normal);
    } else if keys.just_pressed(KeyCode::Digit2) {
        speed.set(SpeedPreset::Fast);
    } else if keys.just_pressed(KeyCode::Digit3) {
        speed.set(SpeedPreset::VeryFast);
    }
}

const TIMING_WINDOW_CAP: usize = 200;
const TIMING_EMA_ALPHA: f32 = 0.05;

/// Per-tick CPU timing + ticks-per-frame counter. Populated by:
/// - [`fixed_tick_timing_start_system`] (FixedUpdate, first in tick)
/// - [`fixed_tick_timing_end_system`] (FixedUpdate, last in tick)
/// - [`frame_tick_count_system`] (Update, once per render frame)
///
/// `worst_tick_us_p99` is the load-bearing spike metric. EMA hides the rare
/// burst; worst-of-window catches it once; p99 across the 200-tick window
/// shows the *upper-tail* shape that the user actually feels as freezes.
#[derive(Resource, Default)]
pub struct SimTimingDiagnostics {
    pub fixed_ticks_this_frame: u32,
    pub avg_tick_us_ema: f32,
    pub worst_tick_us_recent: u32,
    pub worst_tick_us_p99: u32,
    pub recent_worst_window: VecDeque<u32>,
}

/// Shared tick start timestamp. Written by [`fixed_tick_timing_start_system`]
/// at the head of each FixedUpdate tick; read+cleared by
/// [`fixed_tick_timing_end_system`] at the tail.
#[derive(Resource, Default)]
pub struct TickTimer {
    pub last_start: Option<Instant>,
}

pub fn fixed_tick_timing_start_system(mut timer: ResMut<TickTimer>) {
    timer.last_start = Some(Instant::now());
}

pub fn fixed_tick_timing_end_system(
    mut timer: ResMut<TickTimer>,
    mut diag: ResMut<SimTimingDiagnostics>,
) {
    let Some(t0) = timer.last_start.take() else {
        return;
    };
    let dt_us = t0.elapsed().as_micros().min(u32::MAX as u128) as u32;
    if diag.avg_tick_us_ema == 0.0 {
        diag.avg_tick_us_ema = dt_us as f32;
    } else {
        diag.avg_tick_us_ema += TIMING_EMA_ALPHA * (dt_us as f32 - diag.avg_tick_us_ema);
    }
    diag.recent_worst_window.push_back(dt_us);
    if diag.recent_worst_window.len() > TIMING_WINDOW_CAP {
        diag.recent_worst_window.pop_front();
    }
    diag.worst_tick_us_recent = diag.recent_worst_window.iter().copied().max().unwrap_or(0);
    diag.worst_tick_us_p99 = compute_p99(&diag.recent_worst_window);
}

/// p99 from the rolling tick-time window. With a 200-tick window this is the
/// 198th-ranked sample — the *upper tail*, not the single worst outlier.
fn compute_p99(window: &VecDeque<u32>) -> u32 {
    if window.is_empty() {
        return 0;
    }
    let mut sorted: Vec<u32> = window.iter().copied().collect();
    sorted.sort_unstable();
    let idx = ((sorted.len() as f32) * 0.99).floor() as usize;
    let idx = idx.min(sorted.len() - 1);
    sorted[idx]
}

/// Per-render-frame: how many fixed ticks ran since the last Update? Bevy
/// runs 0+ FixedUpdate iterations per Update depending on `Time<Virtual>`
/// accumulator + `relative_speed`. This is the operator-facing number — at
/// 5× speed and 60 fps you should see ~1.67/frame on average.
pub fn frame_tick_count_system(
    clock: Res<SimClock>,
    mut diag: ResMut<SimTimingDiagnostics>,
    mut last_seen: Local<u64>,
) {
    let cur = clock.tick;
    diag.fixed_ticks_this_frame = cur.saturating_sub(*last_seen) as u32;
    *last_seen = cur;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::test_fixture::TestSim;
    use crate::simulation::SimClock;
    use crate::world::tile::TileKind;

    // --- Pure-data unit tests on the enum / resource ------------------------

    #[test]
    fn pause_toggle_round_trip_preserves_preset() {
        let mut s = GameSpeed::default();
        s.set(SpeedPreset::Fast);
        assert_eq!(s.current, SpeedPreset::Fast);
        s.toggle_pause();
        assert_eq!(s.current, SpeedPreset::Paused);
        assert_eq!(s.last_unpaused, SpeedPreset::Fast);
        s.toggle_pause();
        assert_eq!(s.current, SpeedPreset::Fast);
    }

    #[test]
    fn preset_multipliers() {
        assert_eq!(SpeedPreset::Paused.multiplier(), 0.0);
        assert_eq!(SpeedPreset::Normal.multiplier(), 1.0);
        assert_eq!(SpeedPreset::Fast.multiplier(), 2.0);
        assert_eq!(SpeedPreset::VeryFast.multiplier(), 5.0);
    }

    #[test]
    fn set_paused_keeps_last_unpaused() {
        let mut s = GameSpeed::default();
        s.set(SpeedPreset::VeryFast);
        s.set(SpeedPreset::Paused);
        assert_eq!(s.current, SpeedPreset::Paused);
        assert_eq!(s.last_unpaused, SpeedPreset::VeryFast);
    }

    // --- Integration tests via the headless TestSim harness -----------------
    //
    // `TestSim` uses `TimeUpdateStrategy::ManualDuration(50ms)` so each
    // `tick()` advances real time by exactly one fixed-tick. At a given
    // `relative_speed = N`, `Time<Virtual>` advances `N × 50ms` per tick,
    // and FixedUpdate fires roughly `N` times per `tick()`.

    fn flat_with_agent(seed: u64) -> TestSim {
        let mut sim = TestSim::new(seed);
        sim.flat_world(2, 0, TileKind::Grass);
        // Spawn one agent so `SimClock.population > 0` (otherwise
        // `advance_sim_clock` early-returns and the tick counter never
        // advances).
        let _ = sim.spawn_person(0, (0, 0), |_| {});
        // One settling tick so the agent registers.
        sim.tick();
        sim
    }

    #[test]
    fn speed_paused_halts_sim_clock() {
        let mut sim = flat_with_agent(1);
        sim.app
            .world_mut()
            .resource_mut::<GameSpeed>()
            .set(SpeedPreset::Paused);
        // First tick after the set propagates the pause via the sync
        // system; subsequent ticks should not advance the clock.
        sim.tick();
        let baseline = sim.app.world().resource::<SimClock>().tick;
        sim.tick_n(10);
        let after = sim.app.world().resource::<SimClock>().tick;
        assert_eq!(
            after,
            baseline,
            "Paused: SimClock advanced by {} ticks (should be 0)",
            after - baseline
        );
    }

    #[test]
    fn speed_2x_fires_more_fixed_ticks_than_1x() {
        let mut sim_1x = flat_with_agent(2);
        let mut sim_2x = flat_with_agent(2);
        sim_2x
            .app
            .world_mut()
            .resource_mut::<GameSpeed>()
            .set(SpeedPreset::Fast);
        // Burn one tick so the sync system propagates GameSpeed →
        // `Time<Virtual>::relative_speed`.
        sim_1x.tick();
        sim_2x.tick();
        let base_1x = sim_1x.tick_count();
        let base_2x = sim_2x.tick_count();
        sim_1x.tick_n(20);
        sim_2x.tick_n(20);
        let d_1x = sim_1x.tick_count() - base_1x;
        let d_2x = sim_2x.tick_count() - base_2x;
        // 2× should run roughly 2× the fixed ticks per `tick()`.
        assert!(
            d_2x as f32 / d_1x.max(1) as f32 >= 1.6,
            "2× ran {} fixed ticks vs 1× {} (ratio {:.2}, expected ≥1.6)",
            d_2x,
            d_1x,
            d_2x as f32 / d_1x.max(1) as f32,
        );
    }

    #[test]
    fn speed_5x_fires_about_5x_more_fixed_ticks() {
        let mut sim_1x = flat_with_agent(3);
        let mut sim_5x = flat_with_agent(3);
        sim_5x
            .app
            .world_mut()
            .resource_mut::<GameSpeed>()
            .set(SpeedPreset::VeryFast);
        sim_1x.tick();
        sim_5x.tick();
        let base_1x = sim_1x.tick_count();
        let base_5x = sim_5x.tick_count();
        sim_1x.tick_n(20);
        sim_5x.tick_n(20);
        let d_1x = sim_1x.tick_count() - base_1x;
        let d_5x = sim_5x.tick_count() - base_5x;
        let ratio = d_5x as f32 / d_1x.max(1) as f32;
        assert!(
            ratio >= 4.0,
            "5× ran {} fixed ticks vs 1× {} (ratio {:.2}, expected ≥4.0)",
            d_5x,
            d_1x,
            ratio,
        );
    }

    #[test]
    fn resume_from_pause_restores_prior_speed() {
        let mut sim = flat_with_agent(4);
        sim.app
            .world_mut()
            .resource_mut::<GameSpeed>()
            .set(SpeedPreset::Fast);
        sim.tick();
        sim.app
            .world_mut()
            .resource_mut::<GameSpeed>()
            .toggle_pause();
        sim.tick();
        assert_eq!(
            sim.app.world().resource::<GameSpeed>().current,
            SpeedPreset::Paused
        );
        sim.app
            .world_mut()
            .resource_mut::<GameSpeed>()
            .toggle_pause();
        sim.tick();
        assert_eq!(
            sim.app.world().resource::<GameSpeed>().current,
            SpeedPreset::Fast,
            "Resume should restore the prior non-paused preset"
        );
    }

    #[test]
    fn diagnostics_record_after_ticks() {
        let mut sim = flat_with_agent(5);
        sim.tick_n(30);
        let diag = sim.app.world().resource::<SimTimingDiagnostics>();
        assert!(
            diag.avg_tick_us_ema > 0.0,
            "Expected avg_tick_us_ema > 0 after 30 ticks, got {}",
            diag.avg_tick_us_ema
        );
        assert!(
            !diag.recent_worst_window.is_empty(),
            "Expected the worst-tick window to have samples"
        );
    }
}
