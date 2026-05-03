use super::construction::{enclosure_score, BedMap, CampfireMap, ChairMap, TableMap};
use super::lod::LodLevel;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use super::stats::{self, Stats};
use super::tasks::{task_is_labor, TaskKind};
use crate::world::chunk::ChunkMap;
use crate::world::terrain::TILE_SIZE;
use bevy::prelude::*;

/// Hunger at or above this value triggers Survive-driven eating from inventory.
/// Below this, agents won't interrupt other work to eat — preserves food for
/// when it's actually needed and prevents over-consumption of high-nutrition
/// foods at low hunger.
pub const EAT_TRIGGER_HUNGER: u8 = 180;

/// 7 u8 needs + 1 padding = 8 bytes.
#[derive(Component, Clone, Copy, Default)]
#[repr(C)]
pub struct Needs {
    pub hunger: f32,
    pub sleep: f32,
    pub shelter: f32,
    pub safety: f32,
    pub social: f32,
    pub reproduction: f32,
    // NOTE: inverted polarity vs the other needs. 0 = drained (needs play),
    // 255 = full vigor. Drains while the agent works; refills via the Play
    // task and while sleeping (bed doubles the rate). `worst()` and
    // `avg_distress()` add (255 - willpower) so it still counts as distress
    // when low.
    pub willpower: f32,
}

impl Needs {
    pub fn new(
        hunger: f32,
        sleep: f32,
        shelter: f32,
        safety: f32,
        social: f32,
        willpower: f32,
    ) -> Self {
        Self {
            hunger,
            sleep,
            shelter,
            safety,
            social,
            reproduction: 0.0,
            willpower,
        }
    }

    pub fn worst(&self) -> f32 {
        self.hunger
            .max(self.sleep)
            .max(self.shelter)
            .max(self.safety)
            .max(self.social)
            .max(255.0 - self.willpower)
    }

    pub fn avg_distress(&self) -> f32 {
        (self.hunger
            + self.sleep
            + self.shelter
            + self.safety
            + self.social
            + self.reproduction
            + (255.0 - self.willpower))
            / 7.0
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
/// Willpower drain while in a labor task (per real second).
const WILLPOWER_WORK_DRAIN: f32 = 0.6;
/// Baseline willpower drift while idle / not working (per real second).
const WILLPOWER_IDLE_DRAIN: f32 = 0.05;
/// Willpower regen while AiState::Sleeping (per real second). Doubled when
/// the sleeper is on a bed, mirroring the bed bonus on SLEEP_RECOVER_RATE.
const WILLPOWER_SLEEP_RECOVER: f32 = 1.0;

pub fn tick_needs_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    bed_map: Res<BedMap>,
    campfire_map: Res<CampfireMap>,
    table_map: Res<TableMap>,
    chair_map: Res<ChairMap>,
    mut query: Query<(
        &BucketSlot,
        &mut Needs,
        &mut PersonAI,
        &LodLevel,
        &Transform,
        Option<&Stats>,
    )>,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    query
        .par_iter_mut()
        .for_each(|(slot, mut needs, mut ai, lod, transform, stats)| {
            if *lod == LodLevel::Dormant {
                return;
            }
            if !clock.is_active(slot.0) {
                return;
            }

            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

            // CON modifier slows hunger and sleep decay; floor at 0.25× so a
            // very high CON can't make an agent immortal.
            let con_scale = stats
                .map(|s| (1.0 - 0.05 * stats::modifier(s.constitution) as f32).max(0.25))
                .unwrap_or(1.0);

            needs.hunger = (needs.hunger + HUNGER_RATE * dt * con_scale).clamp(0.0, 255.0);

            if ai.state == AiState::Sleeping {
                // Double sleep recovery when resting on a bed.
                let on_bed = bed_map.0.contains_key(&(cur_tx as i32, cur_ty as i32));
                let recovery = if on_bed {
                    SLEEP_RECOVER_RATE * 2.0
                } else {
                    SLEEP_RECOVER_RATE
                };
                needs.sleep = (needs.sleep - recovery * dt).clamp(0.0, 255.0);
                let willpower_gain = if on_bed {
                    WILLPOWER_SLEEP_RECOVER * 2.0
                } else {
                    WILLPOWER_SLEEP_RECOVER
                };
                needs.willpower =
                    (needs.willpower + willpower_gain * dt).clamp(0.0, 255.0);
                if needs.sleep < 10.0 {
                    ai.state = AiState::Idle;
                }
            } else {
                needs.sleep = (needs.sleep + SLEEP_RATE * dt * con_scale).clamp(0.0, 255.0);
            }

            // Shelter fills when the agent is enclosed by walls or higher terrain.
            let enc = enclosure_score(&chunk_map, cur_tx, cur_ty) as f32;
            let shelter_fill = enc * SHELTER_FILL_PER_SCORE * dt;
            needs.shelter = (needs.shelter + SHELTER_RATE * dt - shelter_fill).clamp(0.0, 255.0);

            // Campfire warmth: agents within 3 Manhattan tiles of a campfire
            // gain shelter and safety relief — fire keeps the cold and predators away.
            const CAMPFIRE_WARMTH: f32 = 0.25;
            let near_fire = campfire_map
                .0
                .keys()
                .any(|&cpos| (cpos.0 as i32 - cur_tx).abs() + (cpos.1 as i32 - cur_ty).abs() <= 3);
            if near_fire {
                needs.shelter = (needs.shelter - CAMPFIRE_WARMTH * dt).clamp(0.0, 255.0);
                needs.safety = (needs.safety - CAMPFIRE_WARMTH * 0.5 * dt).clamp(0.0, 255.0);
            }

            needs.safety = (needs.safety + SAFETY_RATE * dt).clamp(0.0, 255.0);
            needs.social = (needs.social + SOCIAL_RATE * dt).clamp(0.0, 255.0);

            // Table+Chair social bonus: agents in the Socialize task within
            // one tile of both a Table and a Chair recover social need 2× faster.
            if ai.task_id == TaskKind::Socialize as u16 {
                let tile = (cur_tx as i32, cur_ty as i32);
                let mut near_table = false;
                let mut near_chair = false;
                for dy in -1..=1i32 {
                    for dx in -1..=1i32 {
                        let p = (tile.0 + dx, tile.1 + dy);
                        if !near_table && table_map.0.contains_key(&p) {
                            near_table = true;
                        }
                        if !near_chair && chair_map.0.contains_key(&p) {
                            near_chair = true;
                        }
                    }
                }
                if near_table && near_chair {
                    const SOCIAL_FURNITURE_BONUS: f32 = SOCIAL_RATE * 2.0;
                    needs.social = (needs.social - SOCIAL_FURNITURE_BONUS * dt).clamp(0.0, 255.0);
                }
            }

            needs.reproduction = (needs.reproduction + REPRODUCTION_RATE * dt).clamp(0.0, 255.0);

            // Willpower: drains while doing labor, drifts down slowly otherwise.
            // Refill is applied separately by `play_system` so it can scale by
            // the entertainment value of the agent's chosen target. Sleeping
            // agents don't drain — rest is restorative.
            if ai.state != AiState::Sleeping
                && ai.task_id != TaskKind::Play as u16
                && ai.task_id != TaskKind::PlayPlant as u16
                && ai.task_id != TaskKind::PlayThrow as u16
            {
                let drain = if task_is_labor(ai.task_id) {
                    WILLPOWER_WORK_DRAIN
                } else {
                    WILLPOWER_IDLE_DRAIN
                };
                needs.willpower = (needs.willpower - drain * dt).clamp(0.0, 255.0);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_worst() {
        let n = Needs::new(10.0, 200.0, 50.0, 30.0, 5.0, 200.0);
        assert_eq!(n.worst(), 200.0);
    }

    #[test]
    fn needs_saturate() {
        let mut n = Needs::new(250.0, 0.0, 0.0, 0.0, 0.0, 200.0);
        n.hunger = (n.hunger + 100.0).clamp(0.0, 255.0);
        assert_eq!(n.hunger, 255.0);
    }

    #[test]
    fn drained_willpower_dominates_worst() {
        // willpower=10 → contributes 245 to worst, larger than any other field.
        let n = Needs::new(50.0, 100.0, 30.0, 30.0, 50.0, 10.0);
        assert_eq!(n.worst(), 245.0);
    }
}
