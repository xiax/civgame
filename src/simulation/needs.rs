use bevy::prelude::*;
use super::schedule::{BucketSlot, SimClock};
use super::lod::LodLevel;

/// 6 u8 needs + 2 padding = 8 bytes.
#[derive(Component, Clone, Copy, Default)]
#[repr(C)]
pub struct Needs {
    pub hunger:       u8,
    pub sleep:        u8,
    pub shelter:      u8,
    pub safety:       u8,
    pub social:       u8,
    pub reproduction: u8,
    _pad: [u8; 2],
}

impl Needs {
    pub fn new(hunger: u8, sleep: u8, shelter: u8, safety: u8, social: u8) -> Self {
        Self { hunger, sleep, shelter, safety, social, reproduction: 0, _pad: [0; 2] }
    }

    pub fn worst(&self) -> u8 {
        self.hunger
            .max(self.sleep)
            .max(self.shelter)
            .max(self.safety)
            .max(self.social)
    }

    pub fn avg_distress(&self) -> f32 {
        (self.hunger as f32
            + self.sleep as f32
            + self.shelter as f32
            + self.safety as f32
            + self.social as f32
            + self.reproduction as f32)
            / 6.0
    }
}

/// Rates in need-units per real second.
const HUNGER_RATE:       f32 = 4.0;
const SLEEP_RATE:        f32 = 2.0;
const SHELTER_RATE:      f32 = 0.5;
const SAFETY_RATE:       f32 = 0.3;
const SOCIAL_RATE:       f32 = 1.0;
const REPRODUCTION_RATE: f32 = 0.3;

pub fn tick_needs_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    mut query: Query<(&BucketSlot, &mut Needs, &LodLevel)>,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    query.par_iter_mut().for_each(|(slot, mut needs, lod)| {
        if *lod == LodLevel::Dormant {
            return;
        }
        if !clock.is_active(slot.0) {
            return;
        }
        needs.hunger       = needs.hunger.saturating_add((HUNGER_RATE       * dt) as u8);
        needs.sleep        = needs.sleep.saturating_add((SLEEP_RATE         * dt) as u8);
        needs.shelter      = needs.shelter.saturating_add((SHELTER_RATE     * dt) as u8);
        needs.safety       = needs.safety.saturating_add((SAFETY_RATE       * dt) as u8);
        needs.social       = needs.social.saturating_add((SOCIAL_RATE       * dt) as u8);
        needs.reproduction = needs.reproduction.saturating_add((REPRODUCTION_RATE * dt) as u8);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_worst() {
        let n = Needs::new(10, 200, 50, 30, 5);
        assert_eq!(n.worst(), 200);
    }

    #[test]
    fn needs_saturate() {
        let mut n = Needs::new(250, 0, 0, 0, 0);
        n.hunger = n.hunger.saturating_add(100);
        assert_eq!(n.hunger, 255);
    }
}
