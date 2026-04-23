use bevy::prelude::*;
use super::schedule::{BucketSlot, SimClock};
use super::lod::LodLevel;

/// 6 u8 needs + 2 padding = 8 bytes.
#[derive(Component, Clone, Copy, Default)]
#[repr(C)]
pub struct Needs {
    pub hunger:       f32,
    pub sleep:        f32,
    pub shelter:      f32,
    pub safety:       f32,
    pub social:       f32,
    pub reproduction: f32,
}

impl Needs {
    pub fn new(hunger: f32, sleep: f32, shelter: f32, safety: f32, social: f32) -> Self {
        Self { hunger, sleep, shelter, safety, social, reproduction: 0.0 }
    }

    pub fn worst(&self) -> f32 {
        self.hunger
            .max(self.sleep)
            .max(self.shelter)
            .max(self.safety)
            .max(self.social)
    }

    pub fn avg_distress(&self) -> f32 {
        (self.hunger
            + self.sleep
            + self.shelter
            + self.safety
            + self.social
            + self.reproduction)
            / 6.0
    }
}

/// Rates in need-units per real second.
const HUNGER_RATE:       f32 = 0.4;
const SLEEP_RATE:        f32 = 0.5;
const SHELTER_RATE:      f32 = 0.1;
const SAFETY_RATE:       f32 = 0.1;
const SOCIAL_RATE:       f32 = 0.2;
const REPRODUCTION_RATE: f32 = 0.1;

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
        needs.hunger       = (needs.hunger + HUNGER_RATE * dt).clamp(0.0, 255.0);
        needs.sleep        = (needs.sleep + SLEEP_RATE * dt).clamp(0.0, 255.0);
        needs.shelter      = (needs.shelter + SHELTER_RATE * dt).clamp(0.0, 255.0);
        needs.safety       = (needs.safety + SAFETY_RATE * dt).clamp(0.0, 255.0);
        needs.social       = (needs.social + SOCIAL_RATE * dt).clamp(0.0, 255.0);
        needs.reproduction = (needs.reproduction + REPRODUCTION_RATE * dt).clamp(0.0, 255.0);
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
