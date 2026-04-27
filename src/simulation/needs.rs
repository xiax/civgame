use super::construction::{enclosure_score, BedMap};
use super::lod::LodLevel;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use crate::world::chunk::ChunkMap;
use crate::world::terrain::TILE_SIZE;
use bevy::prelude::*;

/// 6 u8 needs + 2 padding = 8 bytes.
#[derive(Component, Clone, Copy, Default)]
#[repr(C)]
pub struct Needs {
    pub hunger: f32,
    pub sleep: f32,
    pub shelter: f32,
    pub safety: f32,
    pub social: f32,
    pub reproduction: f32,
}

impl Needs {
    pub fn new(hunger: f32, sleep: f32, shelter: f32, safety: f32, social: f32) -> Self {
        Self {
            hunger,
            sleep,
            shelter,
            safety,
            social,
            reproduction: 0.0,
        }
    }

    pub fn worst(&self) -> f32 {
        self.hunger
            .max(self.sleep)
            .max(self.shelter)
            .max(self.safety)
            .max(self.social)
    }

    pub fn avg_distress(&self) -> f32 {
        (self.hunger + self.sleep + self.shelter + self.safety + self.social + self.reproduction)
            / 6.0
    }
}

/// Rates in need-units per real second.
const HUNGER_RATE: f32 = 0.07;
const SLEEP_RATE: f32 = 0.5;
const SLEEP_RECOVER_RATE: f32 = 2.0;
const SHELTER_RATE: f32 = 0.1;
const SHELTER_FILL_PER_SCORE: f32 = 0.15; // filled per enclosure-score point per second
const SAFETY_RATE: f32 = 0.1;
const SOCIAL_RATE: f32 = 0.2;
const REPRODUCTION_RATE: f32 = 0.1;

pub fn tick_needs_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    bed_map: Res<BedMap>,
    mut query: Query<(
        &BucketSlot,
        &mut Needs,
        &mut PersonAI,
        &LodLevel,
        &Transform,
    )>,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    query
        .par_iter_mut()
        .for_each(|(slot, mut needs, mut ai, lod, transform)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !clock.is_active(slot.0) {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

            needs.hunger = (needs.hunger + HUNGER_RATE * dt).clamp(0.0, 255.0);

            if ai.state == AiState::Sleeping {
                // Double sleep recovery when resting on a bed.
                let on_bed = bed_map.0.contains_key(&(cur_tx as i16, cur_ty as i16));
                let recovery = if on_bed {
                    SLEEP_RECOVER_RATE * 2.0
                } else {
                    SLEEP_RECOVER_RATE
                };
                needs.sleep = (needs.sleep - recovery * dt).clamp(0.0, 255.0);
                if needs.sleep < 10.0 {
                    ai.state = AiState::Idle;
                }
            } else {
                needs.sleep = (needs.sleep + SLEEP_RATE * dt).clamp(0.0, 255.0);
            }

            // Shelter fills when the agent is enclosed by walls or higher terrain.
            let enc = enclosure_score(&chunk_map, cur_tx, cur_ty) as f32;
            let shelter_fill = enc * SHELTER_FILL_PER_SCORE * dt;
            needs.shelter = (needs.shelter + SHELTER_RATE * dt - shelter_fill).clamp(0.0, 255.0);

            needs.safety = (needs.safety + SAFETY_RATE * dt).clamp(0.0, 255.0);
            needs.social = (needs.social + SOCIAL_RATE * dt).clamp(0.0, 255.0);
            needs.reproduction = (needs.reproduction + REPRODUCTION_RATE * dt).clamp(0.0, 255.0);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_worst() {
        let n = Needs::new(10.0, 200.0, 50.0, 30.0, 5.0);
        assert_eq!(n.worst(), 200.0);
    }

    #[test]
    fn needs_saturate() {
        let mut n = Needs::new(250.0, 0.0, 0.0, 0.0, 0.0);
        n.hunger = (n.hunger + 100.0).clamp(0.0, 255.0);
        assert_eq!(n.hunger, 255.0);
    }
}
