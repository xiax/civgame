use bevy::prelude::*;

/// Assigned at spawn. Used for staggered bucket updates.
#[derive(Component, Clone, Copy)]
pub struct BucketSlot(pub u32);

/// Governs staggered simulation updates.
/// Each frame processes `bucket_size` entities, cycling through the population.
///
/// Game-speed and pause now live on [`crate::simulation::speed::GameSpeed`],
/// which drives `Time<Virtual>::set_relative_speed` / `pause`. FixedUpdate
/// fires more often per real second at higher speeds, so per-tick `dt` stays
/// 1/20 s and every system that ticks once per fixed update scales naturally.
#[derive(Resource)]
pub struct SimClock {
    pub tick: u64,
    pub bucket_size: u32,
    pub population: u32,
    pub current_start: u32,
    pub current_end: u32,
}

impl Default for SimClock {
    fn default() -> Self {
        Self {
            tick: 0,
            bucket_size: 250,
            population: 0,
            current_start: 0,
            current_end: 0,
        }
    }
}

impl SimClock {
    pub fn is_active(&self, slot: u32) -> bool {
        if self.population == 0 {
            return true;
        }
        slot >= self.current_start && slot < self.current_end
    }

    /// Bucket compensation: how many ticks pass between updates for any
    /// single entity, given `bucket_size` of the population is active per
    /// tick. Multiplied into `dt` so per-entity decay/cooldown rates are
    /// invariant to bucket assignment. Speed is **not** part of this — it
    /// rides on `Time<Virtual>::relative_speed` and shows up as more
    /// FixedUpdate firings per real second.
    pub fn scale_factor(&self) -> f32 {
        if self.population == 0 || self.bucket_size == 0 {
            return 1.0;
        }
        (self.population as f32 / self.bucket_size as f32).max(1.0)
    }
}

pub fn advance_sim_clock(mut clock: ResMut<SimClock>) {
    if clock.population == 0 {
        return;
    }
    let bucket = clock.bucket_size.min(clock.population);
    let next_start = clock.current_end % clock.population;
    let next_end = (next_start + bucket).min(clock.population);
    clock.current_start = next_start;
    clock.current_end = next_end;
    clock.tick = clock.tick.wrapping_add(1);
}
